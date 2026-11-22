//! `tts`: omp's speech-generation tool (`pi-coding-agent/src/tools/tts.ts`).
//!
//! The tool speaks `text` into a file at `output_path`. Candidates come from the
//! `speech` model role and its `retry.fallbackChains.speech` tail, and the backend
//! is picked per candidate from its catalog `api`:
//!
//! * `openai-speech` - `POST {baseUrl}/audio/speech` (`model`, `input`,
//!   `response_format`, `voice`). MP3 or WAV; a sample rate or bit rate is a hard
//!   error rather than a silently dropped parameter.
//! * `xai-tts` - `POST {baseUrl}/tts` (`text`, `voice_id`, `language`, and an
//!   `output_format` only when the caller moved off the endpoint defaults). MP3 or
//!   WAV; 15 000 characters, matching the schema cap.
//! * `local-inference` - on-device Phonon synthesis, always WAV (`PCM16`). This build
//!   ships no on-device worker, so the branch reports upstream's own
//!   "local synthesis failed" error instead of inventing audio; the WAV encoder it
//!   would need is here and tested.
//!
//! Ordering: an MP3 request tries unconfigured cloud candidates before unconfigured
//! local ones, while explicitly configured slots keep their position. A chain that
//! reaches local synthesis for a non-`.wav` path writes the sibling `.wav` file and
//! says so in the result.
//!
//! Wiring outside this module (not done here, and the module is `dead_code` until
//! it is):
//! - `crates/omp-cli/src/tools.rs`: `mod tts;` plus `tts::tool(env.clone())` in the
//!   builtin table, and `"tts" => "speechgen.enabled"` in `enabled_by_settings`.
//! - `crates/omp-cli/src/approval.rs`: a `"tts"` arm in `tool_decision`
//!   (`Tier::Write`, as upstream's `approval: "write"`) and in `approval_details`.
//!
//! Not ported, each reported as a skip or a hard error instead of a guess: the
//! on-device Phonon worker and its download manifest, the catalog
//! `roleDefaults.speech` chain an unconfigured role would fall back to (no such
//! table, and none of its three models, in this workspace), and `models.offline`
//! (absent from this workspace's settings schema, so the offline provider
//! restriction stays inert; its session-provider leg also has no session model to
//! compare against).

// Not wired into `tools.rs` yet (see the module docs): every item below is reached
// from the tests or from the two-line wiring described above.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use omp_providers::{AgentDir, ResolvedModel};
use omp_settings::Settings;
use pa_agent::abort::{aborted_error, throw_if_aborted, AbortSignal};
use pa_agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolResultContent};
use pa_agent::BoxFut;
use serde::Serialize;
use serde_json::{json, Value};

use super::util::{format_path_relative_to_cwd, resolve_to_cwd};
use super::Env;

/// Tool name as the model sees it.
pub const NAME: &str = "tts";

/// `XAI_BUILTIN_VOICES`: xAI's Tier-1 voices. The schema does not enum-restrict
/// `voice_id` because xAI also accepts custom ids; this list drives the
/// description only.
pub const XAI_BUILTIN_VOICES: [&str; 5] = ["ara", "eve", "leo", "rex", "sal"];

/// `DEFAULT_XAI_VOICE_ID`.
pub const DEFAULT_XAI_VOICE_ID: &str = "eve";

/// `DEFAULT_XAI_LANGUAGE`.
pub const DEFAULT_XAI_LANGUAGE: &str = "auto";

/// `DEFAULT_XAI_SAMPLE_RATE`.
pub const DEFAULT_XAI_SAMPLE_RATE: i64 = 24_000;

/// `DEFAULT_XAI_BIT_RATE`.
pub const DEFAULT_XAI_BIT_RATE: i64 = 128_000;

/// `XAI_MAX_TEXT_LENGTH`, which is also the schema's `maxLength`.
pub const XAI_MAX_TEXT_LENGTH: usize = 15_000;

/// `DEFAULT_TTS_VOICE`: the default Phonon voice.
pub const DEFAULT_TTS_VOICE: &str = "harper";

/// `TTS_LOCAL_VOICES` (the curated Phonon catalog), in upstream order.
pub const TTS_LOCAL_VOICES: [&str; 12] = [
    "harper", "hadley", "delia", "everly", "easton", "preston", "miles", "trevor", "maeve",
    "bryony", "edwin", "hugh",
];

/// `SPEECH_TIMEOUT_MS`: one synthesis call is a single request/response body.
const SPEECH_TIMEOUT: Duration = Duration::from_millis(60_000);

/// `detail.slice(0, 300)` in the transport's HTTP error.
const MAX_ERROR_DETAIL_CHARS: usize = 300;

// ---------------------------------------------------------------- schema

/// The `tts` parameter schema (`ttsSchema`).
#[must_use]
pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "text": {
                "type": "string",
                "minLength": 1,
                "maxLength": XAI_MAX_TEXT_LENGTH
            },
            // No schema default: an explicit "eve" must stay distinguishable from
            // an omitted voice (xAI applies its own default; OpenAI-compatible
            // gateways forward the voice only when the caller set one).
            "voice_id": {"type": "string"},
            "language": {"type": "string"},
            "output_path": {"type": "string"},
            "sample_rate": {"type": "integer"},
            "bit_rate": {"type": "integer"}
        },
        "required": ["text", "output_path"]
    })
}

fn voice_list(voices: &[&str], default: &str) -> String {
    voices
        .iter()
        .map(|voice| {
            if *voice == default {
                format!("{voice} (default)")
            } else {
                (*voice).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The tool description: `prompts/tools/tts.md` with its three substitutions
/// (`localVoices`, `xaiVoices`, `maxLength`, the last formatted the way upstream's
/// `toLocaleString("en-US")` does).
#[must_use]
pub fn description() -> String {
    let local_voices = voice_list(&TTS_LOCAL_VOICES, DEFAULT_TTS_VOICE);
    let xai_voices = voice_list(&XAI_BUILTIN_VOICES, DEFAULT_XAI_VOICE_ID);
    let max_length = "15,000";
    format!(concat!(
        "Generate a speech audio file from text and write it to output_path.\n",
        "\n",
        "Models are selected through the speech role and its fallback chain:\n",
        "- local-inference: on-device neural TTS (Phonon 31M, native runtime)\n",
        "  - no network; always WAV/PCM16\n",
        "  - voice set by tts.localVoice — {local_voices}\n",
        "- xai-tts: xAI Grok Voice; MP3 or WAV\n",
        "  - built-in voices: {xai_voices}; custom voice IDs accepted\n",
        "- openai-speech: the selected model's compatible cloud endpoint; MP3 or WAV\n",
        "  - voice_id is forwarded only when explicitly set\n",
        "\n",
        "Ordering and output:\n",
        "- MP3 request: unconfigured cloud candidates are tried before unconfigured local candidates; explicitly configured speech candidates keep their order.\n",
        "- Chain reaches local for a non-WAV path → output is written to the sibling .wav file.\n",
        "- Cloud codec is inferred from the output_path suffix.\n",
        "- Max {max_length} characters.\n"
        ),
        local_voices = local_voices,
        xai_voices = xai_voices,
        max_length = max_length,
    )
}
// ---------------------------------------------------------------- parameters

/// `tts` parameters (`ttsSchema`).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct SpeechParams {
    pub text: String,
    #[serde(default)]
    pub voice_id: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    pub output_path: String,
    // `number.integer` in the schema, i.e. a JS number: `i64` keeps a negative or
    // zero value representable so the api-specific guards can reject it by name
    // instead of the parser failing first.
    #[serde(default)]
    pub sample_rate: Option<i64>,
    #[serde(default)]
    pub bit_rate: Option<i64>,
}

impl SpeechParams {
    /// Parse raw tool arguments.
    ///
    /// # Errors
    /// A missing/mistyped argument, or a `text` outside 1..=15000 characters.
    pub fn from_value(value: &Value) -> Result<Self> {
        let params: Self = serde_json::from_value(value.clone())
            .map_err(|error| anyhow!("Invalid tts arguments: {error}"))?;
        params.validate()?;
        Ok(params)
    }

    /// Reject what the schema advertises but a hand-written call can still get
    /// wrong.
    ///
    /// # Errors
    /// Empty or over-long `text`, or an empty `output_path`.
    pub fn validate(&self) -> Result<()> {
        let length = self.text.chars().count();
        if length == 0 {
            bail!("Invalid tts arguments: text must not be empty");
        }
        if length > XAI_MAX_TEXT_LENGTH {
            bail!(
                "Invalid tts arguments: text is {length} characters; the limit is {XAI_MAX_TEXT_LENGTH}"
            );
        }
        if self.output_path.trim().is_empty() {
            bail!("Invalid tts arguments: output_path must not be empty");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- codec

/// Output container. `SPEECH_FORMATS` narrowed to the two the tool writes: the
/// suffix picks it, and anything that is not `.wav` is MP3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    Wav,
    Mp3,
}

impl Codec {
    /// `outputPath.toLowerCase().endsWith(".wav") ? "wav" : "mp3"`.
    #[must_use]
    pub fn for_path(path: &Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some(extension) if extension.eq_ignore_ascii_case("wav") => Self::Wav,
            _ => Self::Mp3,
        }
    }

    /// `SpeechFormat` as the wire spells it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Mp3 => "mp3",
        }
    }

    /// `SPEECH_FORMAT_MIME_TYPES`.
    #[must_use]
    pub fn mime_type(self) -> &'static str {
        match self {
            Self::Wav => "audio/wav",
            Self::Mp3 => "audio/mpeg",
        }
    }
}

// ---------------------------------------------------------------- candidates

/// `resolveLocalWavPath`: local synthesis always writes WAV, so a non-`.wav`
/// request is rewritten to a sibling `.wav` and flagged.
#[must_use]
pub fn resolve_local_wav_path(output_path: &str) -> (String, bool) {
    if output_path.to_ascii_lowercase().ends_with(".wav") {
        return (output_path.to_string(), false);
    }
    let slash = output_path.rfind('/').max(output_path.rfind('\\'));
    let base = match output_path.rfind('.') {
        Some(dot) if slash.is_none_or(|slash| dot > slash) => &output_path[..dot],
        _ => output_path,
    };
    (format!("{base}.wav"), true)
}

/// One speech candidate: a resolved model plus whether its slot was configured
/// explicitly (`RoleChainCandidate.explicit`).
#[derive(Debug, Clone)]
pub struct SpeechCandidate {
    pub model: ResolvedModel,
    pub explicit: bool,
}

/// Whether the `speech` role itself is configured (`modelRoles.speech`).
fn role_is_explicit(settings: &Settings) -> bool {
    settings
        .model_roles()
        .get("speech")
        .is_some_and(|selector| !selector.trim().is_empty())
}

/// Whether `retry.fallbackChains.speech` is a configured array
/// (`hasConfiguredFallbackChain`); its entries are then explicit too.
fn fallbacks_are_explicit(settings: &Settings) -> bool {
    settings
        .get("retry.fallbackChains")
        .get("speech")
        .is_some_and(Value::is_array)
}

/// `resolveSpeechCandidates`: the `speech` role's model followed by its
/// `retry.fallbackChains.speech` tail, every explicitly configured slot in place.
/// An MP3 request moves unconfigured candidates cloud-first while explicit slots
/// keep their index.
///
/// Divergence: upstream also falls back to the catalog's `roleDefaults.speech`
/// chain (`local/phonon-31m`, `xai/grok-tts`, `xai-oauth/grok-tts`) when the role is
/// unconfigured. This workspace has no role-default chain table and none of those
/// three models in its catalog, so an unconfigured role yields no candidate and the
/// tool reports "No available speech model matches the speech role." instead of
/// resolving a model that cannot be called.
#[must_use]
pub fn resolve_speech_candidates(
    settings: &Settings,
    dir: &AgentDir,
    wants_mp3: bool,
) -> Vec<SpeechCandidate> {
    let primary_explicit = role_is_explicit(settings);
    let fallback_explicit = fallbacks_are_explicit(settings);
    let Ok(primary) = omp_providers::resolve_in(dir, "speech") else {
        return Vec::new();
    };
    let mut ordered = vec![primary.clone()];
    for selector in &primary.fallbacks {
        if selector.trim().is_empty() {
            continue;
        }
        if let Ok(model) = omp_providers::resolve_in(dir, selector) {
            ordered.push(model);
        }
    }
    let mut candidates: Vec<SpeechCandidate> = Vec::new();
    for (index, model) in ordered.into_iter().enumerate() {
        if candidates
            .iter()
            .any(|c| c.model.provider == model.provider && c.model.id == model.id)
        {
            continue;
        }
        candidates.push(SpeechCandidate {
            model,
            explicit: if index == 0 {
                primary_explicit
            } else {
                fallback_explicit
            },
        });
    }
    if wants_mp3 {
        candidates = cloud_first(candidates);
    }
    candidates
}

/// The MP3 reordering: unconfigured candidates become cloud-first, and explicit
/// ones keep their exact position.
fn cloud_first(candidates: Vec<SpeechCandidate>) -> Vec<SpeechCandidate> {
    let non_explicit: Vec<SpeechCandidate> = candidates
        .iter()
        .filter(|candidate| !candidate.explicit)
        .cloned()
        .collect();
    let mut reordered: Vec<SpeechCandidate> = non_explicit
        .iter()
        .filter(|candidate| candidate.model.api != "local-inference")
        .cloned()
        .collect();
    reordered.extend(
        non_explicit
            .into_iter()
            .filter(|candidate| candidate.model.api == "local-inference"),
    );
    let mut next = 0;
    candidates
        .into_iter()
        .map(|candidate| {
            if candidate.explicit {
                return candidate;
            }
            let replacement = reordered
                .get(next)
                .cloned()
                .unwrap_or_else(|| candidate.clone());
            next += 1;
            replacement
        })
        .collect()
}

// ---------------------------------------------------------------- local WAV

const WAV_HEADER_BYTES: usize = 44;
const PCM16_FORMAT: u16 = 1;
const BITS_PER_SAMPLE: u16 = 16;
const WAV_CHANNELS: u16 = 1;

/// `encodeWav`: a canonical 44-byte RIFF/WAVE header followed by little-endian
/// signed 16-bit mono samples, from normalized `[-1, 1]` float samples.
///
/// Quantization matches the JS reference (`Math.round`, i.e. `floor(x + 0.5)`,
/// with the negative half scaled by `-INT16_MIN`) so a sample lands on the same
/// integer here as upstream.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    reason = "header fields are 16/32-bit by definition; the byte counts are clip-length bounded"
)]
pub fn encode_wav(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let block_align = WAV_CHANNELS * (BITS_PER_SAMPLE / 8);
    let byte_rate = sample_rate.saturating_mul(u32::from(block_align));
    let data_bytes = samples.len() * usize::from(BITS_PER_SAMPLE / 8);
    let mut out = Vec::with_capacity(WAV_HEADER_BYTES + data_bytes);

    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(WAV_HEADER_BYTES as u32 - 8 + data_bytes as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&PCM16_FORMAT.to_le_bytes());
    out.extend_from_slice(&WAV_CHANNELS.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&BITS_PER_SAMPLE.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_bytes as u32).to_le_bytes());

    for sample in samples {
        out.extend_from_slice(&quantize(*sample).to_le_bytes());
    }
    out
}

/// One float sample to `PCM16`, clamped before quantization so an out-of-range
/// value cannot wrap.
///
/// The negative half scales by `-INT16_MIN` (32768) and the positive half by
/// `INT16_MAX` (32767), exactly as the reference does, so `-1.0` reaches
/// `i16::MIN` and `1.0` reaches `i16::MAX`. `floor(x + 0.5)` is `Math.round`.
#[allow(
    clippy::cast_possible_truncation,
    reason = "the value is clamped into the i16 range before the cast"
)]
fn quantize(sample: f32) -> i16 {
    let sample = f64::from(sample.clamp(-1.0, 1.0));
    let scaled = if sample < 0.0 {
        sample * 32_768.0
    } else {
        sample * f64::from(i16::MAX)
    };
    let rounded = (scaled + 0.5).floor();
    rounded.clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

/// A decoded local clip (`DecodedAudio` in the upstream worker's protocol).
#[derive(Debug, Clone)]
pub struct LocalAudio {
    pub pcm: Vec<f32>,
    pub sample_rate: u32,
}

/// On-device synthesis. This build ships no Phonon worker, so the branch reports
/// the same failure upstream reports when its worker cannot answer; the encoder
/// above is what a worker would feed.
fn synthesize_local() -> Option<LocalAudio> {
    None
}

/// The `tts.localVoice` value: any non-empty string is forwarded as-is, as
/// `readLocalVoice` does (only a missing or empty value falls back).
///
/// This workspace's schema declares `tts.localVoice` with a stale `af_heart`
/// default, while omp's is [`DEFAULT_TTS_VOICE`]; a settings file that never sets
/// the key therefore reads back `af_heart` here. The value still goes through
/// unchanged, exactly as upstream, so the divergence is in the schema layer and
/// not in this tool.
#[must_use]
pub fn local_voice(settings: &Settings) -> String {
    settings
        .get_string("tts.localVoice")
        .filter(|voice| !voice.is_empty())
        .unwrap_or_else(|| DEFAULT_TTS_VOICE.to_string())
}

// ---------------------------------------------------------------- request bodies

/// `openai-speech` body. A sample rate or bit rate is a validation error, as
/// upstream, rather than a parameter silently dropped on the floor.
///
/// # Errors
/// `sample_rate` or `bit_rate` is set.
pub fn openai_speech_body(model_id: &str, params: &SpeechParams, codec: Codec) -> Result<Value> {
    if params.sample_rate.is_some() {
        bail!("openai-speech does not support sampleRate");
    }
    if params.bit_rate.is_some() {
        bail!("openai-speech does not support bitRate");
    }
    let mut body = json!({
        "model": model_id,
        "input": params.text.as_str(),
        "response_format": codec.as_str()
    });
    if let Some(voice) = params.voice_id.as_deref() {
        body["voice"] = json!(voice);
    }
    Ok(body)
}

/// `xai-tts` body. The endpoint defaults are the wire defaults, so `output_format`
/// appears only when the caller moved off them.
///
/// # Errors
/// `text` over [`XAI_MAX_TEXT_LENGTH`].
pub fn xai_speech_body(params: &SpeechParams, codec: Codec) -> Result<Value> {
    let length = params.text.chars().count();
    if length > XAI_MAX_TEXT_LENGTH {
        bail!("xai-tts input exceeds the {XAI_MAX_TEXT_LENGTH}-character limit");
    }
    let sample_rate = params.sample_rate.unwrap_or(DEFAULT_XAI_SAMPLE_RATE);
    let bit_rate = params.bit_rate.unwrap_or(DEFAULT_XAI_BIT_RATE);
    let mut body = json!({
        "text": params.text.as_str(),
        "voice_id": params.voice_id.as_deref().unwrap_or(DEFAULT_XAI_VOICE_ID),
        "language": params.language.as_deref().unwrap_or(DEFAULT_XAI_LANGUAGE)
    });
    let codec_overridden = codec != Codec::Mp3;
    let sample_rate_overridden = sample_rate != DEFAULT_XAI_SAMPLE_RATE;
    let bit_rate_overridden = codec == Codec::Mp3 && bit_rate != DEFAULT_XAI_BIT_RATE;
    if codec_overridden || sample_rate_overridden || bit_rate_overridden {
        let mut output_format = serde_json::Map::new();
        output_format.insert("codec".to_string(), json!(codec.as_str()));
        // The *defaulted* rate is emitted once the block is entered, so a plain
        // WAV request carries `sample_rate: 24000`.
        if sample_rate > 0 {
            output_format.insert("sample_rate".to_string(), json!(sample_rate));
        }
        if codec == Codec::Mp3 && bit_rate > 0 {
            output_format.insert("bit_rate".to_string(), json!(bit_rate));
        }
        body["output_format"] = Value::Object(output_format);
    }
    Ok(body)
}

// ---------------------------------------------------------------- transport

/// The two failure classes the candidate loop survives (`MissingApiKeyError`,
/// `ProviderHttpError`). Everything else is fatal, as upstream rethrows it.
#[derive(Debug)]
pub enum SpeechError {
    /// No credential resolved for the candidate's provider.
    MissingApiKey(String),
    /// The endpoint answered a non-2xx status.
    Http(String),
}

impl std::fmt::Display for SpeechError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingApiKey(message) | Self::Http(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for SpeechError {}

/// The `User-Agent` header. Upstream sends `USER_AGENT` = `omp/${VERSION}`; this
/// build ships as `omt`, and the workspace has no shared agent user-agent
/// constant (`read_fetch` defines its own), so the product name and crate
/// version stand in.
fn user_agent() -> String {
    format!("omt/{}", env!("CARGO_PKG_VERSION"))
}

/// One `postSpeechRequest` call: the api's endpoint path, bearer auth plus the
/// model's own headers, a 60 s budget, and the audio body back.
///
/// # Errors
/// Abort, transport failure, a non-2xx status ([`SpeechError::Http`]), or a missing
/// key ([`SpeechError::MissingApiKey`], worded as upstream's
/// `MissingApiKeyError`).
async fn post_speech(
    model: &ResolvedModel,
    path: &str,
    payload: &Value,
    client: &reqwest::Client,
    signal: &AbortSignal,
) -> Result<Vec<u8>> {
    throw_if_aborted(signal)?;
    let Some(key) = model.api_key.as_deref().filter(|key| !key.is_empty()) else {
        return Err(anyhow::Error::new(SpeechError::MissingApiKey(
            "No API key available".to_string(),
        )));
    };
    let url = format!("{}{path}", model.base_url.trim_end_matches('/'));
    // `JSON.stringify(payload)` on the wire, with Content-Type set exactly once
    // (`RequestBuilder::json` would append a second value after this header).
    let body = serde_json::to_vec(payload)
        .map_err(|error| anyhow!("serializing the speech request failed: {error}"))?;
    let mut builder = client
        .post(url.as_str())
        .bearer_auth(key)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::USER_AGENT, user_agent())
        .body(body);
    for (name, value) in &model.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let label = format!("{}/{}", model.provider, model.id);
    let response = tokio::select! {
        result = builder.send() => {
            result.map_err(|error| anyhow!("{label}: request to {url} failed: {error}"))?
        }
        () = signal.aborted() => return Err(aborted_error()),
    };
    let status = response.status();
    if !status.is_success() {
        let detail = response.text().await.unwrap_or_default();
        let capped: String = detail.chars().take(MAX_ERROR_DETAIL_CHARS).collect();
        return Err(anyhow::Error::new(SpeechError::Http(format!(
            "{label} speech API failed ({}): {capped}",
            status.as_u16()
        ))));
    }
    let bytes = tokio::select! {
        result = response.bytes() => {
            result.map_err(|error| anyhow!("{label}: reading the response body failed: {error}"))?
        }
        () = signal.aborted() => return Err(aborted_error()),
    };
    // No empty-body guard: upstream writes whatever body arrived, so a zero-byte
    // answer is reported as "Saved 0 bytes" rather than invented as an error.
    Ok(bytes.to_vec())
}

// ---------------------------------------------------------------- held files

/// `assertNotHeldOpen`: overwriting a live SQLite store (or its `-shm`/`-wal`)
/// destroys it, so a write to one this process holds open is refused.
///
/// # Errors
/// The target is a SQLite store held open by this process.
pub fn assert_not_held_open(path: &Path) -> Result<()> {
    if !is_sqlite_store(path) || !held_open_by_this_process(path) {
        return Ok(());
    }
    bail!(
        "EBUSY: '{}' is held open by a SQLite connection in this process; overwriting it would destroy it",
        path.display()
    )
}

/// Whether `path` is a SQLite database or one of its sidecar files.
///
/// The sidecars are `<db>-shm`, `<db>-wal` and `<db>-journal` (a hyphen, not an
/// extension), so the check is a file-name suffix rather than
/// [`Path::extension`], which would report `db-shm` for `agent.db-shm`.
fn is_sqlite_store(path: &Path) -> bool {
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            let lower = name.to_ascii_lowercase();
            ["-shm", "-wal", "-journal"]
                .iter()
                .any(|suffix| lower.ends_with(suffix))
        })
    {
        return true;
    }
    std::fs::File::open(path).ok().is_some_and(|mut file| {
        let mut magic = [0u8; 16];
        std::io::Read::read_exact(&mut file, &mut magic).is_ok() && magic == *b"SQLite format 3\0"
    })
}

/// Linux: does this process hold `path` open? (`/proc/self/fd` symlink scan; the
/// upstream check consults the native SQLite lock registry, which this build has no
/// binding for.)
#[cfg(target_os = "linux")]
fn held_open_by_this_process(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    let Ok(real) = std::fs::canonicalize(path) else {
        return false;
    };
    let target = real.as_os_str().as_bytes();
    let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
        return false;
    };
    entries.filter_map(std::result::Result::ok).any(|entry| {
        std::fs::read_link(entry.path())
            .ok()
            .is_some_and(|link| link.as_os_str().as_bytes() == target)
    })
}

#[cfg(not(target_os = "linux"))]
fn held_open_by_this_process(_path: &Path) -> bool {
    false
}

// ---------------------------------------------------------------- results

/// `TtsToolDetails` (the tool result's `details`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TtsDetails {
    pub bytes: usize,
    pub voice_id: String,
    pub codec: Codec,
    pub backend: String,
}

/// One successful synthesis.
#[derive(Debug, Clone)]
pub struct TtsOutcome {
    /// The LLM-facing content.
    pub text: String,
    pub details: TtsDetails,
    /// Where the audio actually landed (differs from the request for local WAV).
    pub path: PathBuf,
}

/// The cloud result line.
#[must_use]
pub fn render_cloud_text(
    bytes: usize,
    display_path: &str,
    model: &ResolvedModel,
    voice_id: &str,
    codec: Codec,
) -> String {
    format!(
        "Saved {bytes} bytes to {display_path} (model={}/{}, voice={voice_id}, codec={}).",
        model.provider,
        model.id,
        codec.as_str()
    )
}

/// The local result line, with the codec-substitution note when the caller asked
/// for a non-WAV container.
#[must_use]
pub fn render_local_text(
    bytes: usize,
    display_path: &str,
    model_id: &str,
    voice: &str,
    sample_rate: u32,
    substituted: bool,
) -> String {
    let note = if substituted {
        " No local MP3 encoder is bundled, so WAV (PCM16) was written instead of the requested container."
    } else {
        ""
    };
    format!(
        "Saved {bytes} bytes to {display_path} (voice={model_id}/{voice}, codec=wav, backend=local, {sample_rate} Hz).{note}"
    )
}

// ---------------------------------------------------------------- context

/// Everything the tool needs from the session that is not in the parameters.
#[derive(Debug, Clone)]
pub struct SpeechContext {
    pub dir: AgentDir,
    pub cwd: PathBuf,
    pub settings: Settings,
    /// The session model's provider, when one is known: the only provider
    /// `models.offline` permits.
    pub active_provider: Option<String>,
}

impl SpeechContext {
    /// The session's context: the default agent dir and `cwd`'s settings layers.
    #[must_use]
    pub fn for_cwd(cwd: &Path) -> Self {
        Self {
            dir: AgentDir::default(),
            cwd: cwd.to_path_buf(),
            settings: Settings::load(cwd),
            active_provider: None,
        }
    }

    /// `cfgOffline`. `models.offline` is not a registered setting in this
    /// workspace, so an unregistered key reads as unset and the restriction stays
    /// inert.
    fn offline(&self) -> bool {
        self.settings.get_bool("models.offline") == Some(true)
    }
}

// ---------------------------------------------------------------- execute

/// Run one `tts` request: resolve the `speech` chain, then try each candidate
/// until one writes the file.
///
/// The two cases upstream returns as `isError` results (no candidate, every
/// candidate failed) surface as `Err` here, per the [`AgentTool`] contract; the
/// message text is upstream's.
///
/// # Errors
/// Invalid parameters, no `speech` candidate, an unsupported candidate api, or every
/// candidate failing (with the accumulated per-candidate reasons).
pub async fn execute(
    params: &SpeechParams,
    ctx: &SpeechContext,
    signal: &AbortSignal,
) -> Result<TtsOutcome> {
    params.validate()?;
    throw_if_aborted(signal)?;

    let output_path = resolve_to_cwd(&params.output_path, &ctx.cwd);
    let codec = Codec::for_path(&output_path);
    let display_path = format_path_relative_to_cwd(&output_path, &ctx.cwd, false);
    let candidates = resolve_speech_candidates(&ctx.settings, &ctx.dir, codec == Codec::Mp3);
    if candidates.is_empty() {
        bail!("No available speech model matches the speech role.");
    }

    let client = reqwest::Client::builder()
        .timeout(SPEECH_TIMEOUT)
        .build()
        .map_err(|error| anyhow!("building the speech HTTP client failed: {error}"))?;

    let mut failures: Vec<String> = Vec::new();
    for candidate in &candidates {
        let model = &candidate.model;
        let name = format!("{}/{}", model.provider, model.id);
        if ctx.offline() && ctx.active_provider.as_deref() != Some(model.provider.as_str()) {
            failures.push(format!(
                "{name}: not the active session provider in offline mode"
            ));
            continue;
        }
        let attempt = match model.api.as_str() {
            "local-inference" => {
                synthesize_local_audio(model, params, &ctx.settings, &output_path, &ctx.cwd)
            }
            "xai-tts" | "openai-speech" => {
                synthesize_cloud(
                    model,
                    params,
                    codec,
                    &output_path,
                    &display_path,
                    &client,
                    signal,
                )
                .await
            }
            other => bail!("Unsupported speech model API: {other}"),
        };
        match attempt {
            Ok(outcome) => return Ok(outcome),
            Err(error) => {
                if signal.is_aborted() {
                    return Err(error);
                }
                if error.downcast_ref::<SpeechError>().is_some() {
                    // Upstream's `failures.push(`${model.provider}/${model.id}: ${error.message}`)`.
                    failures.push(format!("{name}: {error}"));
                    continue;
                }
                return Err(error);
            }
        }
    }

    bail!(
        "Speech synthesis failed for every candidate: {}",
        failures.join("; ")
    )
}

/// One cloud candidate: build the api's payload, post it, write the audio.
async fn synthesize_cloud(
    model: &ResolvedModel,
    params: &SpeechParams,
    codec: Codec,
    output_path: &Path,
    display_path: &str,
    client: &reqwest::Client,
    signal: &AbortSignal,
) -> Result<TtsOutcome> {
    let (path, payload) = if model.api == "xai-tts" {
        ("/tts", xai_speech_body(params, codec)?)
    } else {
        (
            "/audio/speech",
            openai_speech_body(&model.id, params, codec)?,
        )
    };
    let bytes = post_speech(model, path, &payload, client, signal).await?;
    assert_not_held_open(output_path)?;
    write_audio(output_path, &bytes)?;
    let voice_id = params.voice_id.clone().unwrap_or_else(|| {
        if model.api == "xai-tts" {
            DEFAULT_XAI_VOICE_ID.to_string()
        } else {
            "default".to_string()
        }
    });
    let backend = if model.api == "xai-tts" {
        "xai-tts"
    } else {
        "openai-speech"
    };
    Ok(TtsOutcome {
        text: render_cloud_text(bytes.len(), display_path, model, &voice_id, codec),
        details: TtsDetails {
            bytes: bytes.len(),
            voice_id,
            codec,
            backend: backend.to_string(),
        },
        path: output_path.to_path_buf(),
    })
}

/// One local candidate: synthesize, encode WAV, rewrite a non-`.wav` path to its
/// sibling, write.
///
/// # Errors
/// The on-device worker is unavailable, or the write fails.
fn synthesize_local_audio(
    model: &ResolvedModel,
    _params: &SpeechParams,
    settings: &Settings,
    output_path: &Path,
    cwd: &Path,
) -> Result<TtsOutcome> {
    let voice = local_voice(settings);
    let Some(audio) = synthesize_local() else {
        bail!(
            "Local TTS synthesis failed (model={}). The on-device worker may be unavailable or the model download was interrupted.",
            model.id
        );
    };
    let (wav_path, substituted) = resolve_local_wav_path(&output_path.to_string_lossy());
    let wav = encode_wav(&audio.pcm, audio.sample_rate);
    let wav_path = PathBuf::from(wav_path);
    assert_not_held_open(&wav_path)?;
    write_audio(&wav_path, &wav)?;
    let display_path = format_path_relative_to_cwd(&wav_path, cwd, false);
    Ok(TtsOutcome {
        text: render_local_text(
            wav.len(),
            &display_path,
            &model.id,
            &voice,
            audio.sample_rate,
            substituted,
        ),
        details: TtsDetails {
            bytes: wav.len(),
            voice_id: format!("{}/{voice}", model.id),
            codec: Codec::Wav,
            backend: "local-inference".to_string(),
        },
        path: wav_path,
    })
}

/// Write the audio, creating parents and renaming a temp file into place so a
/// failed write never leaves a truncated clip behind.
///
/// # Errors
/// A parent that cannot be created, or a write/rename that fails.
pub fn write_audio(path: &Path, bytes: &[u8]) -> Result<()> {
    use anyhow::Context as _;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let temp = temp_path(path);
    std::fs::write(&temp, bytes).with_context(|| format!("writing {}", temp.display()))?;
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(anyhow!(
            "moving {} into place failed: {error}",
            temp.display()
        ));
    }
    Ok(())
}

fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("tts");
    path.with_file_name(format!(".{name}.{}.part", std::process::id()))
}

// ---------------------------------------------------------------- tool

/// The `tts` tool. Gated by `speechgen.enabled`; `approval: "write"` because it
/// writes a caller-chosen file.
pub struct TtsTool {
    env: Arc<Env>,
    schema: Value,
    description: String,
}

impl TtsTool {
    #[must_use]
    pub fn new(env: Arc<Env>) -> Self {
        Self {
            env,
            schema: schema(),
            description: description(),
        }
    }
}

impl std::fmt::Debug for TtsTool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("TtsTool").finish_non_exhaustive()
    }
}

impl AgentTool for TtsTool {
    fn name(&self) -> &'static str {
        NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> &Value {
        &self.schema
    }

    fn label(&self) -> &'static str {
        "Speech Generation"
    }

    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        params: Value,
        signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFut<'static, Result<AgentToolResult>> {
        let cwd = self.env.cwd.clone();
        Box::pin(async move {
            let params = SpeechParams::from_value(&params)?;
            let ctx = SpeechContext::for_cwd(&cwd);
            let outcome = execute(&params, &ctx, &signal).await?;
            Ok(AgentToolResult {
                content: vec![ToolResultContent::text(outcome.text)],
                details: serde_json::to_value(&outcome.details).unwrap_or(Value::Null),
                terminate: None,
                is_error: None,
            })
        })
    }
}

/// The `tts` tool (one line in the builtin table).
#[must_use]
pub fn tool(env: Arc<Env>) -> Arc<dyn AgentTool> {
    Arc::new(TtsTool::new(env))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    fn params(value: Value) -> SpeechParams {
        SpeechParams::from_value(&value).unwrap()
    }

    fn model(provider: &str, id: &str, api: &str, base_url: &str) -> ResolvedModel {
        ResolvedModel {
            provider: provider.to_string(),
            id: id.to_string(),
            api: api.to_string(),
            base_url: base_url.to_string(),
            api_key: Some("test-key".to_string()),
            credential_id: None,
            headers: Vec::new(),
            context_window: 0,
            max_tokens: 0,
            reasoning: false,
            thinking: None,
            thinking_auto: false,
            thinking_source: omp_providers::ThinkingSource::None,
            selector: format!("{provider}/{id}"),
            role: Some("speech".to_string()),
            fallbacks: Vec::new(),
            retry: omp_providers::RetryConfig::default(),
            model: serde_json::from_value(json!({
                "id": id,
                "name": id,
                "api": api,
                "provider": provider,
                "baseUrl": base_url,
                "reasoning": false,
                "input": ["text"],
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
                "contextWindow": 8_000,
                "maxTokens": 8_000
            }))
            .unwrap(),
        }
    }

    /// One-shot HTTP server: answers `status`/`body`, hands back the raw request.
    fn serve_once(status: u16, body: Vec<u8>) -> (String, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut raw = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let read = socket.read(&mut chunk).unwrap();
                if read == 0 {
                    break;
                }
                raw.extend_from_slice(&chunk[..read]);
                let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&raw[..split]).to_string();
                let length = head
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if raw.len() >= split + 4 + length {
                    break;
                }
            }
            let head = format!(
                "HTTP/1.1 {status} Test\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).unwrap();
            socket.write_all(&body).unwrap();
            let _ = socket.flush();
            String::from_utf8_lossy(&raw).to_string()
        });
        (format!("http://{address}"), handle)
    }

    fn wav_bytes() -> Vec<u8> {
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&36u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&[0u8; 32]);
        bytes
    }

    fn mp3_bytes() -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xFB, 0x90, 0x64];
        bytes.extend_from_slice(&[0u8; 64]);
        bytes
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// One `post_speech` call on a fresh runtime. The client is built inside the
    /// runtime: reqwest's connector wants a reactor in scope.
    fn post_once(
        model: &ResolvedModel,
        path: &str,
        payload: &Value,
        signal: &AbortSignal,
    ) -> Result<Vec<u8>> {
        runtime().block_on(async {
            let client = reqwest::Client::new();
            post_speech(model, path, payload, &client, signal).await
        })
    }

    /// A context pinned to a temp agent dir, so no test reads the real config.
    ///
    /// The config file is `<dir>/config.yml`, the same path `omp_providers`
    /// reads as its global layer, and it is also passed as the settings global
    /// layer: production has one agent dir serving both roles.
    fn context(dir: &Path, config: Option<&str>) -> SpeechContext {
        let path = dir.join("config.yml");
        if let Some(config) = config {
            std::fs::write(&path, config).unwrap();
        }
        SpeechContext {
            dir: AgentDir(dir.to_path_buf()),
            cwd: dir.to_path_buf(),
            settings: Settings::load_from(Some(path.as_path()), dir),
            active_provider: None,
        }
    }

    // ── schema and description ──────────────────────────────────────────────

    #[test]
    fn schema_matches_the_upstream_shape() {
        let schema = schema();
        assert_eq!(schema["type"], json!("object"));
        assert_eq!(schema["required"], json!(["text", "output_path"]));
        // `ttsSchema` is a plain `type({...})`: omptype emits no
        // `additionalProperties` unless the object asks for `reject`.
        assert!(schema.get("additionalProperties").is_none(), "{schema}");
        let properties = schema["properties"].as_object().unwrap();
        assert_eq!(
            properties.keys().cloned().collect::<Vec<_>>(),
            vec![
                "text",
                "voice_id",
                "language",
                "output_path",
                "sample_rate",
                "bit_rate"
            ]
        );
        assert_eq!(properties["text"]["type"], json!("string"));
        assert_eq!(properties["text"]["minLength"], json!(1));
        assert_eq!(properties["text"]["maxLength"], json!(15_000));
        // `number.integer` in TypeBox: an integer, with no enum and no default.
        assert_eq!(properties["sample_rate"], json!({"type": "integer"}));
        assert_eq!(properties["bit_rate"], json!({"type": "integer"}));
        // The schema deliberately leaves voice_id open (xAI takes custom ids).
        assert_eq!(properties["voice_id"], json!({"type": "string"}));
    }

    #[test]
    fn description_renders_the_three_substitutions() {
        let text = description();
        assert!(
            text.starts_with(
                "Generate a speech audio file from text and write it to output_path.\n"
            ),
            "{text}"
        );
        // `prompts/tools/tts.md` ends with a newline; the rendered text keeps it.
        assert!(text.ends_with("- Max 15,000 characters.\n"), "{text}");
        assert!(text.contains("harper (default)"), "{text}");
        assert!(text.contains("eve (default)"), "{text}");
        assert!(text.contains("tts.localVoice"), "{text}");
        assert!(!text.contains('{'), "unrendered placeholder in {text}");
        // the nested bullets keep their two-space indent
        assert!(
            text.contains("\n  - no network; always WAV/PCM16\n"),
            "{text}"
        );
        assert!(
            text.contains("\n  - built-in voices: ara, eve (default), leo, rex, sal; custom voice IDs accepted\n"),
            "{text}"
        );
    }

    #[test]
    fn tool_reports_the_upstream_identity() {
        let dir = tempfile::tempdir().unwrap();
        let env = Arc::new(Env::new(dir.path(), false));
        let tool = tool(env);
        assert_eq!(tool.name(), "tts");
        assert_eq!(tool.label(), "Speech Generation");
        assert_eq!(tool.parameters(), &schema());
    }

    // ── validation ─────────────────────────────────────────────────────────

    #[test]
    fn text_limits_are_enforced() {
        let error = SpeechParams::from_value(&json!({"text": "", "output_path": "a.wav"}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("must not be empty"), "{error}");

        assert!(SpeechParams::from_value(&json!({
            "text": "a".repeat(XAI_MAX_TEXT_LENGTH),
            "output_path": "a.wav"
        }))
        .is_ok());

        let error = SpeechParams::from_value(&json!({
            "text": "a".repeat(XAI_MAX_TEXT_LENGTH + 1),
            "output_path": "a.wav"
        }))
        .unwrap_err()
        .to_string();
        assert!(error.contains("15001"), "{error}");
    }

    #[test]
    fn output_path_is_required_and_non_empty() {
        let missing = SpeechParams::from_value(&json!({"text": "hi"}))
            .unwrap_err()
            .to_string();
        assert!(missing.contains("output_path"), "{missing}");
        let blank = SpeechParams::from_value(&json!({"text": "hi", "output_path": "  "}))
            .unwrap_err()
            .to_string();
        assert!(blank.contains("output_path"), "{blank}");
    }

    #[test]
    fn optional_arguments_stay_distinguishable_from_defaults() {
        let bare = params(json!({"text": "hi", "output_path": "a.mp3"}));
        assert_eq!(bare.voice_id, None);
        assert_eq!(bare.language, None);
        assert_eq!(bare.sample_rate, None);
        assert_eq!(bare.bit_rate, None);

        let full = params(json!({
            "text": "hi",
            "output_path": "a.wav",
            "voice_id": "eve",
            "language": "pt-BR",
            "sample_rate": 16_000,
            "bit_rate": 64_000
        }));
        assert_eq!(full.voice_id.as_deref(), Some("eve"));
        assert_eq!(full.sample_rate, Some(16_000));
        assert_eq!(full.bit_rate, Some(64_000));
    }

    // ── codec and local path ───────────────────────────────────────────────

    #[test]
    fn codec_comes_from_the_suffix_and_defaults_to_mp3() {
        assert_eq!(Codec::for_path(Path::new("a.wav")), Codec::Wav);
        assert_eq!(Codec::for_path(Path::new("a.WAV")), Codec::Wav);
        assert_eq!(Codec::for_path(Path::new("a.mp3")), Codec::Mp3);
        // anything else is MP3, as upstream (it only tests for ".wav")
        assert_eq!(Codec::for_path(Path::new("a.ogg")), Codec::Mp3);
        assert_eq!(Codec::for_path(Path::new("a")), Codec::Mp3);
        assert_eq!(Codec::Wav.as_str(), "wav");
        assert_eq!(Codec::Mp3.as_str(), "mp3");
        assert_eq!(Codec::Mp3.mime_type(), "audio/mpeg");
        assert_eq!(Codec::Wav.mime_type(), "audio/wav");
    }

    #[test]
    fn local_output_rewrites_to_a_sibling_wav() {
        assert_eq!(
            resolve_local_wav_path("/tmp/clip.wav"),
            ("/tmp/clip.wav".to_string(), false)
        );
        assert_eq!(
            resolve_local_wav_path("/tmp/clip.WAV"),
            ("/tmp/clip.WAV".to_string(), false)
        );
        assert_eq!(
            resolve_local_wav_path("/tmp/clip.mp3"),
            ("/tmp/clip.wav".to_string(), true)
        );
        assert_eq!(
            resolve_local_wav_path("/tmp/clip"),
            ("/tmp/clip.wav".to_string(), true)
        );
        // a dot in a directory name is not an extension
        assert_eq!(
            resolve_local_wav_path("/a.b/clip"),
            ("/a.b/clip.wav".to_string(), true)
        );
        assert_eq!(
            resolve_local_wav_path("C:\\audio\\clip.mp3"),
            ("C:\\audio\\clip.wav".to_string(), true)
        );
    }

    // ── WAV encoder ────────────────────────────────────────────────────────

    fn sample_at(wav: &[u8], index: usize) -> i16 {
        let offset = WAV_HEADER_BYTES + index * 2;
        i16::from_le_bytes(wav[offset..offset + 2].try_into().unwrap())
    }

    #[test]
    fn wav_header_is_a_canonical_pcm16_riff() {
        let samples = [0.0f32, 0.5, -0.5, 1.0];
        let wav = encode_wav(&samples, 24_000);
        assert_eq!(wav.len(), WAV_HEADER_BYTES + samples.len() * 2);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes(wav[16..20].try_into().unwrap()), 16);
        assert_eq!(u16::from_le_bytes(wav[20..22].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 24_000);
        // byte rate = rate * channels * bytes per sample; block align = 2
        assert_eq!(u32::from_le_bytes(wav[28..32].try_into().unwrap()), 48_000);
        assert_eq!(u16::from_le_bytes(wav[32..34].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16);
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(
            u32::from_le_bytes(wav[40..44].try_into().unwrap()),
            u32::try_from(samples.len() * 2).unwrap()
        );
        // RIFF size = file size - 8
        assert_eq!(
            usize::try_from(u32::from_le_bytes(wav[4..8].try_into().unwrap())).unwrap(),
            wav.len() - 8
        );
    }

    #[test]
    fn samples_quantize_like_the_reference() {
        let wav = encode_wav(&[0.0, 0.5, -0.5, 1.0, -1.0], 16_000);
        assert_eq!(sample_at(&wav, 0), 0);
        assert_eq!(sample_at(&wav, 1), 16_384);
        assert_eq!(sample_at(&wav, 2), -16_384);
        assert_eq!(sample_at(&wav, 3), i16::MAX);
        assert_eq!(sample_at(&wav, 4), i16::MIN);
    }

    #[test]
    fn out_of_range_samples_clamp_instead_of_wrapping() {
        let wav = encode_wav(&[2.0, -3.0, 1.5, -1.5], 8_000);
        assert_eq!(sample_at(&wav, 0), i16::MAX);
        assert_eq!(sample_at(&wav, 1), i16::MIN);
        assert_eq!(sample_at(&wav, 2), i16::MAX);
        assert_eq!(sample_at(&wav, 3), i16::MIN);
    }

    #[test]
    fn an_empty_clip_is_a_header_only_file() {
        let wav = encode_wav(&[], 44_100);
        assert_eq!(wav.len(), WAV_HEADER_BYTES);
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 0);
        assert_eq!(
            u32::from_le_bytes(wav[4..8].try_into().unwrap()),
            u32::try_from(WAV_HEADER_BYTES - 8).unwrap()
        );
    }

    // ── candidate ordering ─────────────────────────────────────────────────

    fn candidate(api: &str, explicit: bool) -> SpeechCandidate {
        SpeechCandidate {
            model: model("p", api, api, "http://127.0.0.1:9"),
            explicit,
        }
    }

    #[test]
    fn mp3_requests_move_unconfigured_candidates_cloud_first() {
        let candidates = vec![
            candidate("local-inference", false),
            candidate("openai-speech", false),
            candidate("xai-tts", false),
        ];
        let reordered = cloud_first(candidates);
        let ordered: Vec<&str> = reordered.iter().map(|c| c.model.api.as_str()).collect();
        assert_eq!(ordered, vec!["openai-speech", "xai-tts", "local-inference"]);
    }

    #[test]
    fn explicit_candidates_keep_their_position_when_reordering() {
        let candidates = vec![
            candidate("local-inference", true),
            candidate("openai-speech", false),
            candidate("xai-tts", true),
            candidate("local-inference", false),
        ];
        let reordered = cloud_first(candidates);
        let ordered: Vec<(&str, bool)> = reordered
            .iter()
            .map(|c| (c.model.api.as_str(), c.explicit))
            .collect();
        // The explicit slots stay put; the two unconfigured slots are refilled
        // cloud-first from the non-explicit pool.
        assert_eq!(
            ordered,
            vec![
                ("local-inference", true),
                ("openai-speech", false),
                ("xai-tts", true),
                ("local-inference", false),
            ]
        );
    }

    #[test]
    fn an_unconfigured_role_resolves_no_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::load_from(None, dir.path());
        let resolved =
            resolve_speech_candidates(&settings, &AgentDir(dir.path().to_path_buf()), false);
        assert!(resolved.is_empty(), "{resolved:?}");
    }

    // ── request bodies ─────────────────────────────────────────────────────

    #[test]
    fn openai_body_forwards_the_voice_only_when_set() {
        let bare = params(json!({"text": "hello", "output_path": "a.mp3"}));
        let body = openai_speech_body("gpt-4o-mini-tts", &bare, Codec::Mp3).unwrap();
        assert_eq!(body["model"], json!("gpt-4o-mini-tts"));
        assert_eq!(body["input"], json!("hello"));
        assert_eq!(body["response_format"], json!("mp3"));
        assert!(body.get("voice").is_none(), "{body}");

        let voiced = params(json!({
            "text": "hello",
            "output_path": "a.wav",
            "voice_id": "nova"
        }));
        let body = openai_speech_body("gpt-4o-mini-tts", &voiced, Codec::Wav).unwrap();
        assert_eq!(body["voice"], json!("nova"));
        assert_eq!(body["response_format"], json!("wav"));
    }

    #[test]
    fn openai_rejects_the_parameters_it_cannot_honour() {
        let rated = params(json!({
            "text": "hi",
            "output_path": "a.mp3",
            "sample_rate": 16_000
        }));
        let error = openai_speech_body("m", &rated, Codec::Mp3)
            .unwrap_err()
            .to_string();
        assert_eq!(error, "openai-speech does not support sampleRate");

        let bit_rate = params(json!({
            "text": "hi",
            "output_path": "a.mp3",
            "bit_rate": 64_000
        }));
        let error = openai_speech_body("m", &bit_rate, Codec::Mp3)
            .unwrap_err()
            .to_string();
        assert_eq!(error, "openai-speech does not support bitRate");
    }

    #[test]
    fn xai_body_omits_output_format_at_the_endpoint_defaults() {
        let bare = params(json!({"text": "hello", "output_path": "a.mp3"}));
        let body = xai_speech_body(&bare, Codec::Mp3).unwrap();
        assert_eq!(body["text"], json!("hello"));
        assert_eq!(body["voice_id"], json!(DEFAULT_XAI_VOICE_ID));
        assert_eq!(body["language"], json!(DEFAULT_XAI_LANGUAGE));
        assert!(body.get("output_format").is_none(), "{body}");
    }

    #[test]
    fn xai_body_carries_output_format_only_when_overridden() {
        // A WAV request enters the block on `codecOverridden`, and the block then
        // emits the *defaulted* `sample_rate` because 24 000 is positive.
        let wav = params(json!({"text": "hello", "output_path": "a.wav"}));
        let body = xai_speech_body(&wav, Codec::Wav).unwrap();
        assert_eq!(
            body["output_format"],
            json!({"codec": "wav", "sample_rate": DEFAULT_XAI_SAMPLE_RATE})
        );
        // `bit_rate` rides along on MP3 only.
        assert!(body["output_format"].get("bit_rate").is_none(), "{body}");

        let tuned = params(json!({
            "text": "hello",
            "output_path": "a.mp3",
            "language": "en-GB",
            "voice_id": "ara",
            "sample_rate": 48_000,
            "bit_rate": 192_000
        }));
        let body = xai_speech_body(&tuned, Codec::Mp3).unwrap();
        assert_eq!(body["voice_id"], json!("ara"));
        assert_eq!(body["language"], json!("en-GB"));
        assert_eq!(
            body["output_format"],
            json!({"codec": "mp3", "sample_rate": 48_000, "bit_rate": 192_000})
        );

        // A zero rate is omitted (the `> 0` guard) while the block still opens on
        // the codec override; `bit_rate` never rides along on WAV.
        let wav_tuned = params(json!({
            "text": "hello",
            "output_path": "a.wav",
            "sample_rate": 0,
            "bit_rate": 192_000
        }));
        let body = xai_speech_body(&wav_tuned, Codec::Wav).unwrap();
        assert_eq!(body["output_format"], json!({"codec": "wav"}));
    }

    #[test]
    fn xai_enforces_the_character_limit_independently_of_the_schema() {
        let long = SpeechParams {
            text: "a".repeat(XAI_MAX_TEXT_LENGTH + 1),
            output_path: "a.mp3".to_string(),
            ..SpeechParams::default()
        };
        let error = xai_speech_body(&long, Codec::Mp3).unwrap_err().to_string();
        assert_eq!(error, "xai-tts input exceeds the 15000-character limit");
    }

    // ── result text ────────────────────────────────────────────────────────

    #[test]
    fn cloud_result_text_names_model_voice_and_codec() {
        let resolved = model("xai", "grok-tts", "xai-tts", "https://api.x.ai/v1");
        assert_eq!(
            render_cloud_text(1_024, "out/clip.mp3", &resolved, "eve", Codec::Mp3),
            "Saved 1024 bytes to out/clip.mp3 (model=xai/grok-tts, voice=eve, codec=mp3)."
        );
    }

    #[test]
    fn local_result_text_notes_the_codec_substitution() {
        assert_eq!(
            render_local_text(2_048, "out/clip.wav", "phonon-31m", "harper", 24_000, false),
            "Saved 2048 bytes to out/clip.wav (voice=phonon-31m/harper, codec=wav, backend=local, 24000 Hz)."
        );
        let substituted =
            render_local_text(2_048, "out/clip.wav", "phonon-31m", "harper", 24_000, true);
        assert!(
            substituted.ends_with(
                " No local MP3 encoder is bundled, so WAV (PCM16) was written instead of the requested container."
            ),
            "{substituted}"
        );
    }

    // ── settings ───────────────────────────────────────────────────────────

    #[test]
    fn local_voice_forwards_any_non_empty_value() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yml");
        std::fs::write(&config, "tts:\n  localVoice: hadley\n").unwrap();
        assert_eq!(
            local_voice(&Settings::load_from(Some(config.as_path()), dir.path())),
            "hadley"
        );

        // `readLocalVoice` forwards any non-empty string; the curated catalog is
        // only what the settings UI offers, not a filter here.
        std::fs::write(&config, "tts:\n  localVoice: custom-voice\n").unwrap();
        assert_eq!(
            local_voice(&Settings::load_from(Some(config.as_path()), dir.path())),
            "custom-voice"
        );

        // An empty value falls back to the default voice.
        std::fs::write(&config, "tts:\n  localVoice: \"\"\n").unwrap();
        assert_eq!(
            local_voice(&Settings::load_from(Some(config.as_path()), dir.path())),
            DEFAULT_TTS_VOICE
        );
    }

    #[test]
    fn the_gate_setting_exists_and_defaults_off() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::load_from(None, dir.path());
        assert_eq!(settings.get_bool("speechgen.enabled"), Some(false));
    }

    // ── held files ─────────────────────────────────────────────────────────

    #[test]
    fn ordinary_files_are_not_mistaken_for_sqlite_stores() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("clip.wav");
        std::fs::write(&wav, wav_bytes()).unwrap();
        assert!(!is_sqlite_store(&wav));
        assert!(assert_not_held_open(&wav).is_ok());

        let sidecar = dir.path().join("agent.db-shm");
        std::fs::write(&sidecar, [0u8; 4]).unwrap();
        assert!(is_sqlite_store(&sidecar));
        assert!(is_sqlite_store(&dir.path().join("agent.db-wal")));
        assert!(is_sqlite_store(&dir.path().join("agent.db-JOURNAL")));
        // a WAV whose name merely ends in "shm" is not a sidecar
        assert!(!is_sqlite_store(&dir.path().join("clip-shm.wav")));

        let database = dir.path().join("agent.db");
        std::fs::write(&database, b"SQLite format 3\0\0\0").unwrap();
        assert!(is_sqlite_store(&database));
        // not held open by this process, so the write is allowed
        assert!(assert_not_held_open(&database).is_ok());
    }

    // ── execution ──────────────────────────────────────────────────────────

    #[test]
    fn no_speech_role_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path(), None);
        let params = params(json!({"text": "hi", "output_path": "clip.mp3"}));
        let error = runtime()
            .block_on(execute(&params, &ctx, &AbortSignal::never()))
            .unwrap_err()
            .to_string();
        assert_eq!(error, "No available speech model matches the speech role.");
    }

    #[test]
    fn an_unresolvable_configured_role_reports_the_same_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(
            dir.path(),
            Some("modelRoles:\n  speech: not-a-provider/not-a-model\n"),
        );
        let params = params(json!({"text": "hi", "output_path": "clip.mp3"}));
        let error = runtime()
            .block_on(execute(&params, &ctx, &AbortSignal::never()))
            .unwrap_err()
            .to_string();
        assert_eq!(error, "No available speech model matches the speech role.");
    }

    #[test]
    fn only_transport_failures_are_survivable() {
        let fatal = anyhow!("Unsupported speech model API: openai-completions");
        assert!(fatal.downcast_ref::<SpeechError>().is_none());
        let http = anyhow::Error::new(SpeechError::Http("boom".to_string()));
        assert!(http.downcast_ref::<SpeechError>().is_some());
        let key = anyhow::Error::new(SpeechError::MissingApiKey("no key".to_string()));
        assert!(key.downcast_ref::<SpeechError>().is_some());
        assert_eq!(key.to_string(), "no key");
    }

    #[test]
    fn a_200_body_is_posted_with_the_expected_fields_and_written() {
        let dir = tempfile::tempdir().unwrap();
        let (base_url, server) = serve_once(200, mp3_bytes());
        let resolved = model("openai", "gpt-4o-mini-tts", "openai-speech", &base_url);
        let params =
            params(json!({ "text": "hello", "output_path": "clip.mp3", "voice_id": "nova" }));
        let bytes = post_once(
            &resolved,
            "/audio/speech",
            &openai_speech_body(&resolved.id, &params, Codec::Mp3).unwrap(),
            &AbortSignal::never(),
        )
        .unwrap();
        assert_eq!(bytes, mp3_bytes());

        let raw = server.join().unwrap();
        assert!(raw.starts_with("POST /audio/speech HTTP/1.1"), "{raw}");
        assert!(raw.contains("\"model\":\"gpt-4o-mini-tts\""), "{raw}");
        assert!(raw.contains("\"voice\":\"nova\""), "{raw}");
        assert!(raw.contains("\"response_format\":\"mp3\""), "{raw}");
        assert!(
            raw.contains("authorization: Bearer test-key")
                || raw.contains("Authorization: Bearer test-key"),
            "{raw}"
        );

        let target = dir.path().join("out/clip.mp3");
        write_audio(&target, &bytes).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), mp3_bytes());
    }

    #[test]
    fn a_missing_key_is_a_survivable_error() {
        let mut resolved = model("xai", "grok-tts", "xai-tts", "http://127.0.0.1:9/v1");
        resolved.api_key = None;
        let params = params(json!({"text": "hi", "output_path": "a.mp3"}));
        let error = post_once(
            &resolved,
            "/tts",
            &xai_speech_body(&params, Codec::Mp3).unwrap(),
            &AbortSignal::never(),
        )
        .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<SpeechError>(),
            Some(SpeechError::MissingApiKey(_))
        ));
        // `MissingApiKeyError()` with no provider and no override message.
        assert_eq!(error.to_string(), "No API key available");
    }

    #[test]
    fn http_failures_carry_the_status_and_the_body() {
        let (base_url, _server) = serve_once(401, br#"{"error":"bad key"}"#.to_vec());
        let resolved = model("xai", "grok-tts", "xai-tts", &base_url);
        let params = params(json!({"text": "hi", "output_path": "a.mp3"}));
        let error = post_once(
            &resolved,
            "/tts",
            &xai_speech_body(&params, Codec::Mp3).unwrap(),
            &AbortSignal::never(),
        )
        .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<SpeechError>(),
            Some(SpeechError::Http(_))
        ));
        assert_eq!(
            error.to_string(),
            "xai/grok-tts speech API failed (401): {\"error\":\"bad key\"}"
        );
    }

    #[test]
    fn a_zero_byte_body_is_written_rather_than_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (base_url, _server) = serve_once(200, Vec::new());
        let resolved = model("openai", "gpt-4o-mini-tts", "openai-speech", &base_url);
        let params = params(json!({"text": "hi", "output_path": "a.mp3"}));
        let bytes = post_once(
            &resolved,
            "/audio/speech",
            &openai_speech_body(&resolved.id, &params, Codec::Mp3).unwrap(),
            &AbortSignal::never(),
        )
        .unwrap();
        // Upstream writes whatever body arrived; a 0-byte clip is reported, not
        // silently turned into an error.
        assert!(bytes.is_empty(), "{bytes:?}");
        let target = dir.path().join("clip.mp3");
        write_audio(&target, &bytes).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn an_aborted_call_never_reaches_the_network() {
        let controller = pa_agent::abort::AbortController::new();
        controller.abort();
        let resolved = model(
            "openai",
            "gpt-4o-mini-tts",
            "openai-speech",
            "http://127.0.0.1:9",
        );
        let params = params(json!({"text": "hi", "output_path": "a.mp3"}));
        let error = post_once(
            &resolved,
            "/audio/speech",
            &openai_speech_body(&resolved.id, &params, Codec::Mp3).unwrap(),
            &controller.signal(),
        )
        .unwrap_err();
        assert!(pa_agent::abort::is_abort_error(&error), "{error}");
    }

    #[test]
    fn write_audio_creates_parents_and_replaces_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested/deeper/clip.wav");
        write_audio(&target, &wav_bytes()).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), wav_bytes());

        // a second write replaces the content and leaves no temp file behind
        write_audio(&target, b"second").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"second");
        let leftovers: Vec<String> = std::fs::read_dir(target.parent().unwrap())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".part"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn local_synthesis_reports_the_missing_worker() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = model("local", "phonon-31m", "local-inference", "");
        let params = params(json!({"text": "hi", "output_path": "clip.mp3"}));
        let error = synthesize_local_audio(
            &resolved,
            &params,
            &Settings::load_from(None, dir.path()),
            Path::new("clip.mp3"),
            dir.path(),
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            error,
            "Local TTS synthesis failed (model=phonon-31m). The on-device worker may be unavailable or the model download was interrupted."
        );
    }
}
