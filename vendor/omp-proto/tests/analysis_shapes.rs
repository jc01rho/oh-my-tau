//! Exhaustiveness check: the `From` impls below are copied from
//! `omp-shell/src/analysis.rs` with `Str` swapped for `String`. They are
//! written as struct literals with no `..Default::default()`, so this test
//! fails to compile if `omp-proto` gains, loses, renames, or retypes any
//! field that `analysis.rs` reads or constructs.

// The `Str = String` substitution makes `value.x.into()` a no-op that
// `analysis.rs` genuinely needs (its `Str` is a different type). Keeping the
// impls verbatim is the whole point of the fixture, so the lint is allowed.
#![allow(clippy::useless_conversion)]

use omp_proto::omp::policy::v1 as proto;

type Str = String;

#[derive(Default)]
struct ArgIr {
	text: Str,
	dynamism: u32,
	span: Span,
}

#[derive(Default)]
struct PathRefIr {
	lexical: Str,
	resolved: Option<Str>,
	access: u32,
	origin: Str,
	command_index: u32,
	outside_workspace: bool,
	dynamic: bool,
	span: Span,
}

#[derive(Default)]
struct NetRefIr {
	kind: Str,
	direction: Str,
	host: Option<Str>,
	scheme: Option<Str>,
	url: Option<Str>,
	command_index: u32,
	dynamic: bool,
	span: Span,
}

#[derive(Default)]
struct RedirectIr {
	fd: Option<i32>,
	op: Str,
	target_kind: Str,
	target: Option<Str>,
	target_fd: Option<i32>,
	dynamism: u32,
	path: Option<PathRefIr>,
	span: Span,
}

#[derive(Default)]
struct CommandIr {
	index: u32,
	name: Option<Str>,
	argv: Vec<ArgIr>,
	redirects: Vec<RedirectIr>,
	reads: Vec<PathRefIr>,
	writes: Vec<PathRefIr>,
	net: Vec<NetRefIr>,
	cwd: Option<Str>,
	depth: u32,
	subshell: bool,
	interpreter_code: Option<Str>,
	span: Span,
}

#[derive(Default)]
struct ScriptIr {
	source: Str,
	rev: Str,
	commands: Vec<CommandIr>,
	reads: Vec<PathRefIr>,
	writes: Vec<PathRefIr>,
	net: Vec<NetRefIr>,
	opaque: Vec<Str>,
	has_dynamic_eval: bool,
	node_count: u32,
}

#[derive(Clone, Copy, Default)]
struct Span {
	start: u32,
	end: u32,
	line: u32,
	column: u32,
}

// ── omp-shell -> proto (construction) ───────────────────────────────────

impl From<Span> for proto::Span {
	fn from(value: Span) -> Self {
		Self {
			start: value.start,
			end: value.end,
			line: value.line,
			column: value.column,
		}
	}
}

impl From<&ArgIr> for proto::BashArg {
	fn from(value: &ArgIr) -> Self {
		Self {
			text: value.text.to_string(),
			dynamic: value.dynamism != 0,
			dynamism: value.dynamism,
			quoting: String::new(),
			span: Some(value.span.into()),
			props: None,
		}
	}
}

impl From<&PathRefIr> for proto::PathRef {
	fn from(value: &PathRefIr) -> Self {
		Self {
			lexical: value.lexical.to_string(),
			resolved: value.resolved.as_ref().map(ToString::to_string),
			absolute: value.resolved.as_ref().map(ToString::to_string),
			access: value.access,
			origin: value.origin.to_string(),
			command_index: value.command_index,
			outside_workspace: value.outside_workspace,
			exists: false,
			dynamic: value.dynamic,
			span: Some(value.span.into()),
			props: None,
		}
	}
}

impl From<&NetRefIr> for proto::NetRef {
	fn from(value: &NetRefIr) -> Self {
		Self {
			kind: value.kind.to_string(),
			direction: value.direction.to_string(),
			host: value.host.as_ref().map(ToString::to_string),
			port: None,
			scheme: value.scheme.as_ref().map(ToString::to_string),
			url: value.url.as_ref().map(ToString::to_string),
			command_index: value.command_index,
			dynamic: value.dynamic,
			span: Some(value.span.into()),
			props: None,
		}
	}
}

impl From<&RedirectIr> for proto::BashRedirect {
	fn from(value: &RedirectIr) -> Self {
		Self {
			fd: value.fd,
			op: value.op.to_string(),
			target_kind: value.target_kind.to_string(),
			target: value.target.as_ref().map(ToString::to_string),
			target_fd: value.target_fd,
			dynamism: value.dynamism,
			path: value.path.as_ref().map(Into::into),
			span: Some(value.span.into()),
			props: None,
		}
	}
}

impl From<&CommandIr> for proto::BashCommand {
	fn from(value: &CommandIr) -> Self {
		Self {
			index: value.index,
			name: value.name.as_ref().map(ToString::to_string),
			argv: value.argv.iter().map(Into::into).collect(),
			dynamic_args: value.argv.iter().map(|arg| arg.dynamism != 0).collect(),
			redirects: value.redirects.iter().map(Into::into).collect(),
			reads: value.reads.iter().map(Into::into).collect(),
			writes: value.writes.iter().map(Into::into).collect(),
			net: value.net.iter().map(Into::into).collect(),
			cwd: value.cwd.as_ref().map(ToString::to_string),
			depth: value.depth,
			container: String::new(),
			subshell: value.subshell,
			classification: 0,
			interpreter_code: value.interpreter_code.as_ref().map(ToString::to_string),
			span: Some(value.span.into()),
			props: None,
		}
	}
}

impl From<&ScriptIr> for proto::BashIr {
	fn from(value: &ScriptIr) -> Self {
		Self {
			source: value.source.to_string(),
			rev: value.rev.to_string(),
			parser_rev: String::new(),
			parse_ok: true,
			parse_error: None,
			truncated: false,
			node_count: value.node_count,
			is_compound: value.commands.len() != 1,
			has_dynamic_eval: value.has_dynamic_eval,
			commands: value.commands.iter().map(Into::into).collect(),
			reads: value.reads.iter().map(Into::into).collect(),
			writes: value.writes.iter().map(Into::into).collect(),
			net: value.net.iter().map(Into::into).collect(),
			opaque: value.opaque.iter().map(ToString::to_string).collect(),
			props: None,
		}
	}
}

// ── proto -> omp-shell (reading) ────────────────────────────────────────

impl From<proto::Span> for Span {
	fn from(value: proto::Span) -> Self {
		Self {
			start: value.start,
			end: value.end,
			line: value.line,
			column: value.column,
		}
	}
}

impl From<proto::BashArg> for ArgIr {
	fn from(value: proto::BashArg) -> Self {
		Self {
			text: value.text.into(),
			dynamism: value.dynamism,
			span: value.span.map(Into::into).unwrap_or_default(),
		}
	}
}

impl From<proto::PathRef> for PathRefIr {
	fn from(value: proto::PathRef) -> Self {
		Self {
			lexical: value.lexical.into(),
			resolved: value.resolved.map(Into::into),
			access: value.access,
			origin: value.origin.into(),
			command_index: value.command_index,
			outside_workspace: value.outside_workspace,
			dynamic: value.dynamic,
			span: value.span.map(Into::into).unwrap_or_default(),
		}
	}
}

impl From<proto::NetRef> for NetRefIr {
	fn from(value: proto::NetRef) -> Self {
		Self {
			kind: value.kind.into(),
			direction: value.direction.into(),
			host: value.host.map(Into::into),
			scheme: value.scheme.map(Into::into),
			url: value.url.map(Into::into),
			command_index: value.command_index,
			dynamic: value.dynamic,
			span: value.span.map(Into::into).unwrap_or_default(),
		}
	}
}

impl From<proto::BashRedirect> for RedirectIr {
	fn from(value: proto::BashRedirect) -> Self {
		Self {
			fd: value.fd,
			op: value.op.into(),
			target_kind: value.target_kind.into(),
			target: value.target.map(Into::into),
			target_fd: value.target_fd,
			dynamism: value.dynamism,
			path: value.path.map(Into::into),
			span: value.span.map(Into::into).unwrap_or_default(),
		}
	}
}

impl From<proto::BashCommand> for CommandIr {
	fn from(value: proto::BashCommand) -> Self {
		Self {
			index: value.index,
			name: value.name.map(Into::into),
			argv: value.argv.into_iter().map(Into::into).collect(),
			redirects: value.redirects.into_iter().map(Into::into).collect(),
			reads: value.reads.into_iter().map(Into::into).collect(),
			writes: value.writes.into_iter().map(Into::into).collect(),
			net: value.net.into_iter().map(Into::into).collect(),
			cwd: value.cwd.map(Into::into),
			depth: value.depth,
			subshell: value.subshell,
			interpreter_code: value.interpreter_code.map(Into::into),
			span: value.span.map(Into::into).unwrap_or_default(),
		}
	}
}

impl From<proto::BashIr> for ScriptIr {
	fn from(value: proto::BashIr) -> Self {
		Self {
			source: value.source.into(),
			rev: value.rev.into(),
			commands: value.commands.into_iter().map(Into::into).collect(),
			reads: value.reads.into_iter().map(Into::into).collect(),
			writes: value.writes.into_iter().map(Into::into).collect(),
			net: value.net.into_iter().map(Into::into).collect(),
			opaque: value.opaque.into_iter().map(Into::into).collect(),
			has_dynamic_eval: value.has_dynamic_eval,
			node_count: value.node_count,
		}
	}
}

fn outside_write() -> PathRefIr {
	PathRefIr {
		lexical: "../outside".into(),
		resolved: Some("/outside".into()),
		access: 66,
		origin: "touch".into(),
		command_index: 0,
		outside_workspace: true,
		dynamic: false,
		span: Span::default(),
	}
}

#[test]
fn round_trips_a_script_ir() {
	let ir = ScriptIr {
		source: "touch ../outside".into(),
		rev: "omp.policy.v1".into(),
		commands: vec![CommandIr {
			index: 0,
			name: Some("touch".into()),
			argv: vec![
				ArgIr {
					text: "touch".into(),
					dynamism: 0,
					span: Span::default(),
				},
				ArgIr {
					text: "../outside".into(),
					dynamism: 0,
					span: Span {
						start: 6,
						end: 16,
						line: 1,
						column: 7,
					},
				},
			],
			writes: vec![outside_write()],
			cwd: Some("/work".into()),
			..CommandIr::default()
		}],
		// Mirrors analysis.rs: the flat fact lists are what become
		// `BashIr.reads` / `.writes` / `.net`.
		writes: vec![outside_write()],
		node_count: 1,
		..ScriptIr::default()
	};

	let proto_ir: proto::BashIr = (&ir).into();
	assert_eq!(proto_ir.rev, "omp.policy.v1");
	assert_eq!(proto_ir.commands[0].argv.len(), 2);
	assert_eq!(
		proto_ir.commands[0].argv[1].span.map(|span| span.end),
		Some(16)
	);
	assert_eq!(proto_ir.writes[0].absolute.as_deref(), Some("/outside"));
	assert_eq!(proto_ir.commands[0].dynamic_args, vec![false, false]);

	let back: ScriptIr = proto_ir.into();
	assert_eq!(back.commands[0].argv[1].text, "../outside");
	assert_eq!(back.commands[0].writes[0].access, 66);
	assert_eq!(back.commands[0].cwd.as_deref(), Some("/work"));
}
