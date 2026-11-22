# 확장 API api.rpc.handle 설계

## 1. 현재 상태 (근거)

- ext-host/host.ts:340의 makeApi는 on, events.on, registerTool, registerCommand, registerFlag만 노출한다. api.rpc 네임스페이스 자체가 없다.
- omo-nyang/src/session-move-extension.ts:130-140은 api.events.on("wake_source_state", {source, activeCount})로 상태를 구독한 뒤 api.rpc.handle("omomeow.move-readiness", () => ({ activeSources: [...active] }))를 등록한다. rpc.handle이 undefined이므로 등록 시점에 실패한다.
- omo-nyang/src/omo-feedback.ts:41,50은 extension_request({ name: "omo.feedback.prepare" | "omo.feedback.submit", data })로 호스트에 요청을 보낸다. 이름 기반 요청 경로는 이미 존재한다.
- crates/omp-ext/src/host.rs:127 LoadResult, :431 send_request, :463 request(method, params)가 확장 로드와 요청 전달을 담당한다.
- crates/omp-cli/src/rpc.rs:593 dispatch가 메서드를 분기하고, :203은 code와 errorCode를 동시에 직렬화한다.
- crates/omp-session/src/entries.rs:66-77 TitleSlot(256B, 1행), lib.rs:25 SESSION_VERSION=3, lib.rs:225 session 헤더가 세션 포맷을 규정한다.
- omo-nyang/src/session-patterns.ts:47-76은 .jsonl에서 "message"가 있는 줄만 파싱하고, session-identity.ts:13-31은 첫 줄의 type=="session"과 id를 읽는다.
- memory_apply_patch는 YAML frontmatter(description 필수, read_only:"true"면 수정 차단, expires), 6개 연산, [[path]] 링크, Git 자동 커밋 규약을 가진다.

정리: 요청 경로(request/send_request/dispatch)는 이미 완성되어 있고, 확장 측 수신 등록 지점만 비어 있다.

## 2. 게이트웨이가 요구하는 계약

- 이름: 소문자 점 표기. 확장 소유 접두사 필수(omomeow.*, omo.*). 세그먼트는 [a-z0-9-]+, 전체 64자 이하.
- 등록 시점: 확장 로드 중 1회. 로드 완료 후 등록은 거부하고 LoadResult에 오류로 반환한다.
- 시그니처: (params: unknown) => unknown | Promise<unknown>. params는 JSON 직렬화 가능 값.
- 반환: JSON 직렬화 가능 값. 반환값이 그대로 응답 data가 된다.
- 오류: throw 시 code와 errorCode를 모두 채운다. rpc.rs:203이 두 필드를 동시에 직렬화하므로 한쪽만 채우면 계약 위반이다.
- 중복: 동일 이름 재등록은 오류. 서로 다른 확장이 같은 이름을 쓰면 먼저 등록한 쪽이 이기고 두 번째는 로드 실패.
- 수명: 확장 unload 시 등록 전부 해제. 진행 중 요청은 취소하고 오류 응답.
- 상한: 요청/응답 각 256KB, 타임아웃 5초. 초과 시 오류 응답.
- 대칭성: api.events.on과 동일한 등록/해제 수명주기를 공유한다.

## 3. 설계

**API 표면**

- makeApi에 rpc 네임스페이스를 추가하고 handle(name, handler)만 노출한다.
- handle은 동기 등록 함수다. 이름 검증과 중복 검사를 즉시 수행하고 실패 시 throw한다.
- unhandle은 제공하지 않는다. 해제 단위는 확장 수명이며 부분 해제는 지원하지 않는다.

**레지스트리**

- 호스트 프로세스에 전역 Map<string, { extId, handler }> 하나를 둔다. 키는 등록 이름 그대로다.
- extId는 조회 키가 아니라 메타데이터다. errorCode 조립과 로그에만 쓴다.

**요청 흐름**

1. 호출자가 extension_request({ name, data })를 보낸다.
2. host.rs request가 확장 호스트로 전달한다.
3. 레지스트리 조회. 없으면 code=NOT_FOUND, errorCode="rpc.unknown_method".
4. 핸들러를 await로 실행하고 타임아웃과 크기 상한을 적용한다.
5. 성공은 { code: 0, data }, 실패는 { code, errorCode }.

**rpc.rs dispatch 연동**

- 기존 분기 끝에 ext 네임스페이스 분기를 추가한다.
- 메서드 문자열을 확장 이름으로 직접 쓰지 않고 "ext." 접두사를 강제해 내부 메서드와 충돌을 막는다.
- 응답 조립은 :203 경로를 그대로 재사용해 code와 errorCode가 항상 함께 나가게 한다.

**LoadResult 확장**

- 등록된 rpc 이름 목록을 LoadResult에 담아 CLI가 호출 전에 검증할 수 있게 한다.
- 등록 실패는 로드 실패로 승격한다. 부분 성공 상태를 만들지 않는다.

**세션/메모리 경계**

- 핸들러 응답은 세션 파일을 직접 쓰지 않는다. 세션 쓰기는 호스트 계층만 수행한다.
- TitleSlot은 256B 1행 제약이 있으므로 title 계열 응답은 확장이 아니라 호스트가 절단해 반환한다.
- memory_apply_patch의 read_only 차단과 expires 검사는 핸들러가 아니라 도구 계층에 남긴다.

## 4. 구현 단계 (커밋 단위)

1. feat(ext-host): makeApi에 rpc.handle 추가. 이름 규칙 검증, 중복 검사, 전역 레지스트리 등록까지. 검증: 단위 테스트만으로 통과.
2. feat(omp-ext): LoadResult에 rpc 이름 목록 필드 추가, 등록 실패를 로드 실패로 승격. 검증: 기존 확장 로드 회귀 없음.
3. feat(omp-ext): send_request 경로에서 레지스트리 디스패치 연결. 타임아웃 5초, 256KB 상한, unload 시 취소. 검증: Rust 단위 테스트.
4. feat(omp-cli): rpc.rs dispatch에 ext 분기 추가, code/errorCode 동시 직렬화 보장. 검증: dispatch 테이블 테스트.
5. fix(omo-nyang): session-move-extension 등록 스모크 추가, omo-feedback 이름 규칙을 계약에 맞춤. 검증: 실제 확장 로드 후 호출 성공.
6. test+docs: 계약 테스트, 통합 테스트, 문서 반영. 검증: 전체 스위트 통과.

## 5. 테스트

- 단위(TS): 이름 검증 경계값(64자, 대문자, 접두사 없음), 중복 등록, 등록 후 조회.
- 단위(Rust): 레지스트리 조회 실패 시 code/errorCode 조합, 타임아웃 초과, 256KB 초과 응답.
- 계약 테스트: 핸들러 throw가 code와 errorCode를 모두 채우는지 확인. 한쪽만 채우면 실패로 처리.
- 통합: session-move-extension을 로드해 omomeow.move-readiness 호출, wake_source_state 발행 후 activeSources 갱신 확인.
- 통합: omo-feedback.prepare/submit 두 이름이 등록되고 응답 스키마가 유지되는지 확인.
- 회귀: unload 후 동일 이름 재호출이 NOT_FOUND인지, 재로드 시 재등록이 되는지 확인.
- 세션 회귀: SESSION_VERSION=3 파일을 읽고 쓰는 경로에서 응답이 세션 파일을 건드리지 않는지 확인.

## 6. QA 시나리오

1. 정상: 확장 로드 후 등록 이름으로 호출하면 code=0과 data가 온다.
2. 미등록: 존재하지 않는 이름 호출 시 NOT_FOUND와 rpc.unknown_method가 함께 온다.
3. 중복: 같은 이름을 두 번 등록하면 두 번째 로드가 실패하고 첫 등록은 유지된다.
4. 예외: 핸들러가 throw하면 code와 errorCode가 모두 채워진다.
5. 타임아웃: 5초를 넘기는 핸들러는 취소되고 오류 응답이 온다.
6. 대용량: 256KB를 넘는 응답은 잘리지 않고 오류로 거부된다.
7. 수명: 요청 처리 중 확장을 unload하면 진행 중 요청이 오류로 종료되고 등록이 사라진다.
8. 세션: 세션 로드 상태에서 rpc 응답이 .jsonl 첫 줄 type=="session" 헤더를 변경하지 않는다.
9. 메모리: read_only:"true" 문서에 대해 어떤 rpc 경로로도 수정이 발생하지 않는다.
10. 이벤트 연동: wake_source_state 발행 전 호출은 빈 activeSources, 발행 후 호출은 반영된 목록을 준다.

## 7. 위험과 미확인

- 게이트웨이의 실제 이름 규칙(접두사 강제 여부, 64자 상한)은 코드로 확인되지 않았다. 계약 확정 전 구현하면 재작업 위험이 있다.
- code와 errorCode의 의미 분담이 불명확하다. :203이 동시 직렬화한다는 사실만 확인됐고 값 체계는 미확인.
- 핸들러의 동기/비동기 혼용 시 실행 순서와 동시성 상한이 정의되지 않았다. 단일 큐로 직렬화할지 결정 필요.
- 타임아웃 5초와 256KB 상한은 근거 없는 제안값이다. 실제 확장 요구로 검증해야 한다.
- LoadResult 필드 추가가 기존 소비자와 호환되는지 미확인. 필드 추가는 안전하지만 파서가 엄격하면 깨질 수 있다.
- unload 중 요청 취소가 확장 쪽 Promise를 실제로 중단시키는지 미확인. 취소 후 응답이 늦게 도착하는 경합을 처리해야 한다.
- TitleSlot 256B 절단을 호스트가 하면 확장이 반환한 원문과 응답이 달라진다. 절단 사실을 응답에 표시할지 미정.
- memory_apply_patch의 Git 자동 커밋이 rpc 응답 경로에서 부작용을 만들 수 있다. 커밋을 요청 스코프로 묶는 방안 검토 필요.
