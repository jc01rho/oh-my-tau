//! Dependency-free Rust shapes for the subset of the omp wire protocol that
//! `omp-shell` reads and constructs.
//!
//! These are plain structs, not prost-generated messages: no `prost`/`tonic`
//! dependency, no protobuf runtime. Field names, types, and `Option` presence
//! mirror the `.proto` definitions in `omp/inference/v1/common.proto` and
//! `omp/policy/v1/policy.proto` exactly, so the conversions in
//! `omp-shell::analysis` stay mechanical.
//!
//! `optional` proto3 fields map to `Option<T>`; `repeated` fields map to
//! `Vec<T>`; nested `map<string, Value>` maps to a `BTreeMap` (ordering is
//! irrelevant to the consumers here, and `BTreeMap` keeps the crate `std`-only).

pub mod omp {
	pub mod inference {
		pub mod v1 {
			use std::collections::BTreeMap;

			/// Integer-preserving JSON vocabulary for namespaced extension bags.
			///
			/// Mirrors `omp.inference.v1.Value`. Signed integers use [`ValueKind::Int`];
			/// non-negative JSON integers above `i64` use [`ValueKind::Uint`] rather than
			/// truncating through an IEEE-754 double.
			#[derive(Debug, Clone, PartialEq, Default)]
			pub struct Value {
				pub kind: Option<ValueKind>,
			}

			/// The `oneof kind` arm of [`Value`].
			#[derive(Debug, Clone, PartialEq)]
			pub enum ValueKind {
				/// The JSON `null` literal (the proto field is a `bool`).
				Null,
				Int(i64),
				Double(f64),
				Bool(bool),
				String(String),
				List(ValueList),
				Map(ValueMap),
				Uint(u64),
			}

			/// Array-shaped extension data; values recursively retain their numeric kind.
			#[derive(Debug, Clone, PartialEq, Default)]
			pub struct ValueList {
				pub values: Vec<Value>,
			}

			/// Object-shaped extension data. Top-level keys use `namespace/name` form;
			/// nested object keys retain their vendor-defined shape.
			#[derive(Debug, Clone, PartialEq, Default)]
			pub struct ValueMap {
				pub fields: BTreeMap<String, Value>,
			}
		}
	}

	pub mod policy {
		pub mod v1 {
			use super::super::inference::v1::ValueMap;

			/// Portable source coordinates. Offsets are UTF-8 byte offsets.
			#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
			pub struct Span {
				pub start: u32,
				pub end: u32,
				pub line: u32,
				pub column: u32,
			}

			/// One shell word before expansion.
			#[derive(Debug, Clone, PartialEq, Default)]
			pub struct BashArg {
				pub text: String,
				pub dynamic: bool,
				/// Bitmask: parameter=1, command substitution=2, arithmetic=4,
				/// tilde=8, glob=16, brace=32, escape=64.
				pub dynamism: u32,
				pub quoting: String,
				pub span: Option<Span>,
				pub props: Option<ValueMap>,
			}

			/// A filesystem fact inferred from a shell command.
			#[derive(Debug, Clone, PartialEq, Default)]
			pub struct PathRef {
				pub lexical: String,
				pub resolved: Option<String>,
				pub absolute: Option<String>,
				/// Bitmask: read=1, write=2, append=4, exec=8, delete=16,
				/// metadata=32, create=64.
				pub access: u32,
				pub origin: String,
				pub command_index: u32,
				pub outside_workspace: bool,
				pub exists: bool,
				pub dynamic: bool,
				pub span: Option<Span>,
				pub props: Option<ValueMap>,
			}

			/// A network fact inferred from a shell command.
			#[derive(Debug, Clone, PartialEq, Default)]
			pub struct NetRef {
				pub kind: String,
				pub direction: String,
				pub host: Option<String>,
				pub port: Option<u32>,
				pub scheme: Option<String>,
				pub url: Option<String>,
				pub command_index: u32,
				pub dynamic: bool,
				pub span: Option<Span>,
				pub props: Option<ValueMap>,
			}

			/// One shell redirection and its inferred path effect.
			#[derive(Debug, Clone, PartialEq, Default)]
			pub struct BashRedirect {
				pub fd: Option<i32>,
				pub op: String,
				pub target_kind: String,
				pub target: Option<String>,
				pub target_fd: Option<i32>,
				pub dynamism: u32,
				pub path: Option<PathRef>,
				pub span: Option<Span>,
				pub props: Option<ValueMap>,
			}

			/// One flattened command in execution order.
			#[derive(Debug, Clone, PartialEq, Default)]
			pub struct BashCommand {
				pub index: u32,
				pub name: Option<String>,
				pub argv: Vec<BashArg>,
				pub dynamic_args: Vec<bool>,
				pub redirects: Vec<BashRedirect>,
				pub reads: Vec<PathRef>,
				pub writes: Vec<PathRef>,
				pub net: Vec<NetRef>,
				pub cwd: Option<String>,
				pub depth: u32,
				pub container: String,
				pub subshell: bool,
				pub classification: u32,
				pub interpreter_code: Option<String>,
				pub span: Option<Span>,
				pub props: Option<ValueMap>,
			}

			/// Environment-produced shell analysis: a fact record, not an
			/// enforcement token and not a serialization of the parser AST.
			#[derive(Debug, Clone, PartialEq, Default)]
			pub struct BashIr {
				pub source: String,
				pub rev: String,
				pub parser_rev: String,
				pub parse_ok: bool,
				pub parse_error: Option<String>,
				pub truncated: bool,
				pub node_count: u32,
				pub is_compound: bool,
				pub has_dynamic_eval: bool,
				pub commands: Vec<BashCommand>,
				pub reads: Vec<PathRef>,
				pub writes: Vec<PathRef>,
				pub net: Vec<NetRef>,
				pub opaque: Vec<String>,
				pub props: Option<ValueMap>,
			}
		}
	}
}
