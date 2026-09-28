//! The two ways this binary is started, and the flags that pick one.
//!
//! Hand-rolled rather than `clap`, for the same reason the job document is
//! walked by hand: this binary runs somebody else's shell commands, and
//! every crate in it is attack surface the operator inherits. Two
//! subcommands and six flags do not earn a dependency tree.
//!
//! **No subcommand is a usage error**, not a default. There is nothing
//! useful to do without one — a runner that has not registered has no
//! credential to ask for work with — and an operator who types the bare
//! name wants to be told what it does.
//!
//! `RegisterOpts` deliberately has no `Debug`. It holds a registration
//! token, and a derived one is how a credential ends up in a panic message
//! that gets pasted into an issue — the same argument as [`crate::Config`].

use std::path::PathBuf;

pub const USAGE: &str = "\
usage:
  weft-runner register --url URL --token weftg_TOKEN
                          [--name NAME] [--labels a,b] [--ephemeral] [--dir DIR]
      exchange a registration token for this machine's own credential and
      write DIR/.runner. --name defaults to the hostname, --dir to `.`.

  weft-runner run [--dir DIR]
      read DIR/.runner and keep asking for jobs until stopped.
      STRATUM_RUNNER_MAX_PROCS lowers the ceiling on the processes a
      step may start (default 4096); STRATUM_RUNNER_FLUSH_MS sets how
      often a running job's log is sent, in milliseconds (default 1000).
";

/// What the arguments asked for.
pub enum Command {
    Register(RegisterOpts),
    Run(RunOpts),
    /// `--help`: print [`USAGE`] and exit 0. Asking for the usage is not a
    /// usage error, and an operator who gets exit 2 for `--help` reads it
    /// as the binary being broken.
    Usage,
}

pub struct RegisterOpts {
    pub url: String,
    pub token: String,
    /// `None` means "use the hostname", resolved at registration rather
    /// than here so that parsing stays pure.
    pub name: Option<String>,
    pub labels: Vec<String>,
    pub ephemeral: bool,
    pub dir: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub struct RunOpts {
    pub dir: PathBuf,
}

impl Command {
    /// The payload, when this is the command it says it is.
    ///
    /// An `Option` accessor rather than a `match` written out in each test
    /// helper: the "it was something else" arm is then a value a test can
    /// assert on, instead of a `panic!` on a line no run ever reaches and
    /// the coverage gate is right to ask about.
    #[cfg(test)]
    fn into_register(self) -> Option<RegisterOpts> {
        match self {
            Command::Register(o) => Some(o),
            _ => None,
        }
    }

    #[cfg(test)]
    fn into_run(self) -> Option<RunOpts> {
        match self {
            Command::Run(o) => Some(o),
            _ => None,
        }
    }
}

/// The arguments after the program name.
pub fn parse(args: &[String]) -> Result<Command, String> {
    let Some(first) = args.first() else {
        return Err("a command is needed: register or run".into());
    };
    match first.as_str() {
        "-h" | "--help" | "help" => Ok(Command::Usage),
        "register" => parse_register(&args[1..]).map(Command::Register),
        "run" => parse_run(&args[1..]).map(Command::Run),
        other => Err(format!("unknown command {other:?}")),
    }
}

fn parse_register(args: &[String]) -> Result<RegisterOpts, String> {
    let mut o = RegisterOpts {
        url: String::new(),
        token: String::new(),
        name: None,
        labels: Vec::new(),
        ephemeral: false,
        dir: PathBuf::from("."),
    };
    let mut i = 0;
    while i < args.len() {
        let (flag, inline) = split_flag(&args[i]);
        match flag {
            "--url" => o.url = value(args, &mut i, inline, flag)?,
            "--token" => o.token = value(args, &mut i, inline, flag)?,
            "--name" => o.name = Some(value(args, &mut i, inline, flag)?),
            "--labels" => o.labels = labels(&value(args, &mut i, inline, flag)?),
            "--dir" => o.dir = PathBuf::from(value(args, &mut i, inline, flag)?),
            "--ephemeral" => {
                flagless(inline, flag)?;
                o.ephemeral = true;
            }
            _ => return Err(unknown(flag, "register")),
        }
        i += 1;
    }
    if o.url.is_empty() {
        return Err("register needs --url".into());
    }
    if o.token.is_empty() {
        return Err("register needs --token".into());
    }
    Ok(o)
}

fn parse_run(args: &[String]) -> Result<RunOpts, String> {
    let mut o = RunOpts {
        dir: PathBuf::from("."),
    };
    let mut i = 0;
    while i < args.len() {
        let (flag, inline) = split_flag(&args[i]);
        match flag {
            "--dir" => o.dir = PathBuf::from(value(args, &mut i, inline, flag)?),
            _ => return Err(unknown(flag, "run")),
        }
        i += 1;
    }
    Ok(o)
}

/// `--flag=value` and `--flag value` both. Operators paste both forms out
/// of documentation and shell history, and refusing one of them is a
/// support question rather than a safety property.
fn split_flag(arg: &str) -> (&str, Option<&str>) {
    match arg.split_once('=') {
        Some((k, v)) => (k, Some(v)),
        None => (arg, None),
    }
}

fn value(
    args: &[String],
    i: &mut usize,
    inline: Option<&str>,
    flag: &str,
) -> Result<String, String> {
    if let Some(v) = inline {
        return Ok(v.to_string());
    }
    *i += 1;
    match args.get(*i) {
        Some(v) => Ok(v.clone()),
        None => Err(format!("{flag} needs a value")),
    }
}

/// A switch that was given a value: `--ephemeral=yes` almost always means
/// the writer thought it took one, and silently ignoring it would leave a
/// runner that is not ephemeral when its operator believes it is.
fn flagless(inline: Option<&str>, flag: &str) -> Result<(), String> {
    match inline {
        Some(_) => Err(format!("{flag} takes no value")),
        None => Ok(()),
    }
}

fn unknown(flag: &str, cmd: &str) -> String {
    if flag.starts_with('-') {
        format!("unknown option {flag} for `{cmd}`")
    } else {
        format!("unexpected argument {flag:?} for `{cmd}`")
    }
}

/// `--labels a,B,,a` → `["a", "b"]`.
///
/// Lowercased, trimmed, blanks dropped and duplicates removed with the
/// order kept, because that is what the server stores and a runner whose
/// `.runner` disagreed with the server's list would print labels it does
/// not have. The character set is the server's to enforce: it owns the
/// refusal sentence, and a second copy of the rule here is a second place
/// for the two to drift apart.
fn labels(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in raw.split(',') {
        let l = part.trim().to_ascii_lowercase();
        if !l.is_empty() && !out.contains(&l) {
            out.push(l);
        }
    }
    out
}

/// The OS and architecture as the server names them, from the target this
/// binary was built for — not from `uname`, which reports the kernel a
/// process may be emulated on.
pub fn platform() -> Result<(&'static str, &'static str), String> {
    platform_of(std::env::consts::OS, std::env::consts::ARCH)
}

/// Split from [`platform`] so the mapping — the part that can be wrong —
/// is tested for every value rather than only for whichever machine ran
/// the suite.
fn platform_of(os: &str, arch: &str) -> Result<(&'static str, &'static str), String> {
    let os_label = match os {
        "linux" => "linux",
        "macos" => "macos",
        "windows" => "windows",
        other => return Err(format!("weft-runner does not run on {other}")),
    };
    let arch_label = match arch {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => return Err(format!("weft-runner does not run on {os} {other}")),
    };
    Ok((os_label, arch_label))
}

/// What `--name` defaults to.
pub fn default_name() -> String {
    name_or_fallback(&hostname())
}

/// `gethostname(2)`. The buffer is zeroed and one byte is held back, so
/// the result is NUL-terminated whatever the call does — including
/// failing, which leaves the buffer as it was and is handled as "no
/// hostname" by [`name_or_fallback`] rather than by a branch nothing can
/// reach.
fn hostname() -> String {
    let mut buf = vec![0u8; 256];
    let len = buf.len() - 1;
    unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, len) };
    String::from_utf8_lossy(&buf)
        .trim_end_matches('\0')
        .to_string()
}

fn name_or_fallback(host: &str) -> String {
    let h = host.trim();
    if h.is_empty() {
        // A machine with no hostname still gets a runner. The server makes
        // the name unique within the organisation, so a second nameless
        // one replaces the first — which is the documented meaning of
        // registering under a name that is already live.
        "runner".to_string()
    } else {
        h.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    fn register(s: &str) -> RegisterOpts {
        parse(&args(s))
            .ok()
            .and_then(Command::into_register)
            .expect("a register")
    }

    fn refuse(s: &str) -> String {
        parse(&args(s)).err().expect("must be refused")
    }

    fn run_dir(s: &str) -> PathBuf {
        parse(&args(s))
            .ok()
            .and_then(Command::into_run)
            .expect("a run")
            .dir
    }

    /// No arguments is refused like any other mistyped command line, so
    /// `entry` prints the usage beside it and exits 2.
    #[test]
    fn no_arguments_is_a_usage_error_and_help_is_not() {
        assert_eq!(refuse(""), "a command is needed: register or run");
        assert!(matches!(parse(&args("--help")), Ok(Command::Usage)));
        assert!(matches!(parse(&args("-h")), Ok(Command::Usage)));
        assert!(matches!(parse(&args("help")), Ok(Command::Usage)));
        assert!(USAGE.contains("weft-runner register --url URL"));
        assert!(USAGE.contains("weft-runner run [--dir DIR]"));
    }

    /// The knobs `run` reads are named in the usage, with the defaults
    /// they fall back to — and the default quoted there is the one the
    /// code uses, not a number somebody typed once and the constant
    /// moved away from.
    #[test]
    fn the_usage_names_the_run_knobs_and_their_real_defaults() {
        let defaults = crate::agent::Params::default();
        assert!(USAGE.contains("STRATUM_RUNNER_MAX_PROCS"), "{USAGE}");
        assert!(USAGE.contains("STRATUM_RUNNER_FLUSH_MS"), "{USAGE}");
        assert!(
            USAGE.contains(&format!("(default {})", defaults.max_procs)),
            "{USAGE}"
        );
        assert!(
            USAGE.contains(&format!("(default {})", defaults.log.flush.as_millis())),
            "{USAGE}"
        );
    }

    #[test]
    fn register_takes_its_flags_in_either_spelling() {
        let o = register("register --url http://cp/ --token weftg_x");
        assert_eq!(o.url, "http://cp/");
        assert_eq!(o.token, "weftg_x");
        assert_eq!(o.name, None);
        assert_eq!(o.labels, Vec::<String>::new());
        assert!(!o.ephemeral);
        assert_eq!(o.dir, PathBuf::from("."));

        let o = register(
            "register --url=http://cp --token=weftg_x --name=box1 --labels=gpu,Big --dir=/srv/r",
        );
        assert_eq!(o.url, "http://cp");
        assert_eq!(o.name.as_deref(), Some("box1"));
        assert_eq!(o.labels, vec!["gpu".to_string(), "big".to_string()]);
        assert_eq!(o.dir, PathBuf::from("/srv/r"));
    }

    /// The switch, and the mistake it is most often made with. A
    /// `--ephemeral=false` that quietly registered an ephemeral runner
    /// would de-register the machine after one job and read as the product
    /// losing runners.
    #[test]
    fn ephemeral_is_a_switch_and_says_so_when_it_is_given_a_value() {
        assert!(register("register --url u --token t --ephemeral").ephemeral);
        assert_eq!(
            refuse("register --url u --token t --ephemeral=false"),
            "--ephemeral takes no value"
        );
    }

    #[test]
    fn a_flag_without_its_value_names_the_flag_rather_than_the_position() {
        assert_eq!(refuse("register --url"), "--url needs a value");
        assert_eq!(refuse("register --url u --token"), "--token needs a value");
        assert_eq!(refuse("run --dir"), "--dir needs a value");
    }

    #[test]
    fn the_two_required_flags_are_named_when_they_are_missing() {
        assert_eq!(refuse("register"), "register needs --url");
        assert_eq!(refuse("register --token t"), "register needs --url");
        assert_eq!(refuse("register --url u"), "register needs --token");
    }

    #[test]
    fn an_unknown_command_option_or_stray_word_is_refused_by_name() {
        assert_eq!(refuse("start"), "unknown command \"start\"");
        assert_eq!(
            refuse("register --url u --token t --group g"),
            "unknown option --group for `register`"
        );
        assert_eq!(
            refuse("register --url u --token t extra"),
            "unexpected argument \"extra\" for `register`"
        );
        assert_eq!(refuse("run --url u"), "unknown option --url for `run`");
        assert_eq!(
            refuse("run somewhere"),
            "unexpected argument \"somewhere\" for `run`"
        );
    }

    #[test]
    fn run_defaults_to_the_working_directory() {
        // The accessors answer for the command they are about and not for
        // the other one — which is what lets every test helper above be a
        // chain of expressions rather than a panic arm.
        assert!(parse(&args("run"))
            .ok()
            .and_then(Command::into_register)
            .is_none());
        assert!(parse(&args("register --url u --token t"))
            .ok()
            .and_then(Command::into_run)
            .is_none());
        assert_eq!(run_dir("run"), PathBuf::from("."));
        assert_eq!(run_dir("run --dir /srv/r"), PathBuf::from("/srv/r"));
        assert_eq!(run_dir("run --dir=/srv/r"), PathBuf::from("/srv/r"));
        assert_eq!(
            RunOpts {
                dir: PathBuf::from(".")
            },
            RunOpts {
                dir: PathBuf::from(".")
            },
            "RunOpts compares by directory, which is all it is"
        );
    }

    #[test]
    fn labels_are_lowercased_trimmed_deduplicated_and_kept_in_order() {
        assert_eq!(labels("gpu"), vec!["gpu"]);
        assert_eq!(labels(" GPU , big-mem ,gpu,, "), vec!["gpu", "big-mem"]);
        assert_eq!(labels(""), Vec::<String>::new());
        assert_eq!(labels(" , "), Vec::<String>::new());
    }

    /// The mapping the server routes on. `x86_64` and `aarch64` are Rust's
    /// spellings; `x64` and `arm64` are the server's, and getting the two
    /// crossed means a runner that registers with labels no job can ever
    /// match — a runner that is online forever and never takes work.
    #[test]
    fn the_platform_is_named_the_way_the_server_labels_it() {
        assert_eq!(platform_of("linux", "x86_64"), Ok(("linux", "x64")));
        assert_eq!(platform_of("linux", "aarch64"), Ok(("linux", "arm64")));
        assert_eq!(platform_of("macos", "aarch64"), Ok(("macos", "arm64")));
        assert_eq!(platform_of("windows", "x86_64"), Ok(("windows", "x64")));
        assert_eq!(
            platform_of("freebsd", "x86_64"),
            Err("weft-runner does not run on freebsd".to_string())
        );
        assert_eq!(
            platform_of("linux", "powerpc64"),
            Err("weft-runner does not run on linux powerpc64".to_string())
        );
        // And this machine is one of them, whichever it is.
        let (os, arch) = platform().expect("a supported platform");
        assert!(["linux", "macos", "windows"].contains(&os));
        assert!(["x64", "arm64"].contains(&arch));
    }

    #[test]
    fn the_default_name_is_the_hostname_and_never_empty() {
        assert_eq!(name_or_fallback("  build-box-1 \n"), "build-box-1");
        assert_eq!(name_or_fallback(""), "runner");
        assert_eq!(name_or_fallback("   "), "runner");
        // Whatever this machine is called, it is a usable name.
        let n = default_name();
        assert!(!n.is_empty() && n == n.trim(), "{n:?}");
        assert!(!hostname().contains('\0'), "the buffer is trimmed");
    }
}
