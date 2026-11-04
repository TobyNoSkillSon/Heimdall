//! `heimdall-splash serve`'s options (serve_options.py): the subset Heimdall passes or may want, spelled as Splash
//! spells them (`--flag value` or `--flag=value`), so the engine manager's command line is unchanged.
use anyhow::{bail, ensure, Context, Result};
use std::{ffi::OsString, path::PathBuf, time::Duration};

pub const REASONING_EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];
/// The targets' native window, the most context without --max-context.
pub const MAX_CONTEXT_TOKENS: u32 = 262_144;
pub const MAX_YARN_CONTEXT_TOKENS: u32 = 1_010_000;
pub const DEFAULT_QUEUE_SIZE: usize = 32;
pub const DEFAULT_PORT: u16 = 8000;
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct ServeOptions {
    /// `owner/repo[:VARIANT]`, or a directory already prepared as a model.
    pub model: String,
    pub host: String,
    pub port: u16,
    /// Additional API names; responses report the loaded model id unless `announce_served_name`.
    pub served_model_names: Vec<String>,
    pub announce_served_name: bool,
    pub max_memory: Option<u64>,
    pub max_context: Option<u32>,
    /// None: auto; Some(0): no SSD cache.
    pub max_cache_disk: Option<u64>,
    pub persistent_cache: bool,
    pub cache_dir: Option<PathBuf>,
    /// Seconds; infinity is off. None: the engine's default (10 minutes).
    pub idle_release: Option<f64>,
    pub default_reasoning_effort: Option<String>,
    pub thinking_token_budget: Option<u32>,
    pub request_timeout: Option<Duration>,
    pub queue_size: usize,
    pub max_request_bytes: usize,
    /// Where models are prepared; default `<data>/splash/models`, Heimdall's data folder (docs/home.md).
    pub models_dir: Option<PathBuf>,
    /// The native engine; default `../Resources/splash/engine/splash` beside this binary in an app bundle.
    pub binary: Option<PathBuf>,
    /// Never touch the network: serve only what the Hugging Face cache already holds.
    pub offline: bool,
    pub language_only: bool,
    pub disable_ane: bool,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            model: String::new(),
            host: "127.0.0.1".into(),
            port: DEFAULT_PORT,
            served_model_names: vec![],
            announce_served_name: false,
            max_memory: None,
            max_context: None,
            max_cache_disk: None,
            persistent_cache: false,
            cache_dir: None,
            idle_release: None,
            default_reasoning_effort: None,
            thinking_token_budget: None,
            request_timeout: None,
            queue_size: DEFAULT_QUEUE_SIZE,
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            models_dir: None,
            binary: None,
            offline: false,
            language_only: false,
            disable_ane: false,
        }
    }
}

impl ServeOptions {
    pub fn parse(args: &[OsString]) -> Result<Self> {
        let mut o = Self::default();
        let mut it = args.iter().map(|a| a.to_string_lossy().into_owned()).peekable();
        while let Some(arg) = it.next() {
            let (flag, inline) = match arg.split_once('=') {
                Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
                _ => (arg.clone(), None),
            };
            let switch = |inline: &Option<String>| -> Result<()> {
                ensure!(inline.is_none(), "{flag} takes no value");
                Ok(())
            };
            let mut value = || -> Result<String> {
                match &inline {
                    Some(v) => Ok(v.clone()),
                    None => it.next().with_context(|| format!("{flag} needs a value")),
                }
            };
            match flag.as_str() {
                "--model" => o.model = value()?,
                "--host" => o.host = value()?,
                "--port" => o.port = value()?.parse().context("--port must be in [0, 65535]")?,
                "--served-model-name" => o.served_model_names.push(served_name(&value()?)?),
                "--announce-served-name" => { switch(&inline)?; o.announce_served_name = true }
                "--max-memory" => o.max_memory = size_or_auto(&value()?).context("--max-memory")?,
                "--max-context" => o.max_context = context(&value()?)?,
                "--max-cache-disk" => {
                    let v = value()?;
                    o.max_cache_disk = if v.trim() == "0" { Some(0) } else { size_or_auto(&v).context("--max-cache-disk: use auto, 0 to disable, or a size such as 16G")? }
                }
                "--persistent-cache" => { switch(&inline)?; o.persistent_cache = true }
                "--cache-dir" => {
                    let v = value()?;
                    ensure!(!v.trim().is_empty(), "--cache-dir must name a directory");
                    o.cache_dir = Some(absolute(PathBuf::from(v)));
                }
                "--idle-release" => {
                    let v = value()?;
                    o.idle_release = Some(if v.trim().eq_ignore_ascii_case("off") { f64::INFINITY } else { duration(&v).context("--idle-release must be off or a positive duration such as 30m, 2h or 600")? });
                }
                "--default-reasoning-effort" => {
                    let v = value()?;
                    ensure!(REASONING_EFFORTS.contains(&v.as_str()), "--default-reasoning-effort must be one of {}", REASONING_EFFORTS.join(", "));
                    o.default_reasoning_effort = Some(v);
                }
                "--thinking-token-budget" => o.thinking_token_budget = Some(value()?.parse().context("--thinking-token-budget must be a non-negative integer")?),
                "--request-timeout" => o.request_timeout = Some(Duration::from_secs_f64(duration(&value()?).context("--request-timeout must be a positive duration such as 30m, 2h or 3600")?)),
                "--queue-size" => {
                    o.queue_size = value()?.parse().ok().filter(|n| *n > 0).context("--queue-size must be a positive number of requests such as 32")?
                }
                "--max-request-size" => o.max_request_bytes = size_or_auto(&value()?)?.context("--max-request-size must be a positive byte count such as 128M")? as usize,
                "--models-dir" => o.models_dir = Some(absolute(PathBuf::from(value()?))),
                "--binary" => o.binary = Some(absolute(PathBuf::from(value()?))),
                "--offline" => { switch(&inline)?; o.offline = true }
                "--language-only" => { switch(&inline)?; o.language_only = true }
                "--disable-ane" => { switch(&inline)?; o.disable_ane = true }
                other => bail!("unknown option {other}"),
            }
        }
        ensure!(!o.model.is_empty(), "--model is required");
        ensure!(!o.announce_served_name || !o.served_model_names.is_empty(), "--announce-served-name needs --served-model-name");
        ensure!(!o.persistent_cache || o.max_cache_disk != Some(0), "--persistent-cache needs the SSD cache --max-cache-disk 0 turns off");
        ensure!(o.cache_dir.is_none() || o.persistent_cache, "--cache-dir needs --persistent-cache");
        Ok(o)
    }

    /// The engine's command line serving `model_root` (server.py _native_command).
    pub fn native_command(&self, binary: &std::path::Path, model_root: &std::path::Path) -> Vec<OsString> {
        let mut c: Vec<OsString> = vec![
            binary.into(),
            "serve-native".into(),
            model_root.into(),
            self.max_context.map_or("auto".into(), |n| n.to_string()).into(),
            self.max_memory.map_or("auto".into(), |n| n.to_string()).into(),
        ];
        if let Some(disk) = self.max_cache_disk {
            c.push(disk.to_string().into());
        }
        if self.persistent_cache {
            c.extend(["--persistent-cache".into(), "on".into()]);
        }
        if let Some(dir) = &self.cache_dir {
            c.extend(["--cache-dir".into(), dir.into()]);
        }
        if let Some(idle) = self.idle_release {
            c.extend(["--idle-release".into(), if idle.is_infinite() { "off".into() } else { python_float(idle) }.into()]);
        }
        if self.disable_ane {
            c.extend(["--ane".into(), "off".into()]);
        }
        c
    }
}

/// A float as Python's str() writes it (`600.0`), which the engine's command line has always read.
fn python_float(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e16 { format!("{v:.1}") } else { format!("{v}") }
}

fn absolute(p: PathBuf) -> PathBuf {
    let p = match p.strip_prefix("~") {
        Ok(rest) => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest),
        Err(_) => p,
    };
    if p.is_absolute() { p } else { std::env::current_dir().map(|d| d.join(&p)).unwrap_or(p) }
}

fn served_name(v: &str) -> Result<String> {
    ensure!(!v.trim().is_empty() && v.len() <= 256 && !v.chars().any(char::is_control), "--served-model-name must be a short name");
    Ok(v.to_string())
}

/// 'auto' (None) or a byte count with an optional K/M/G (KB/KIB...) suffix; a suffixed count may have a fraction,
/// rounded down (parse_max_memory).
pub fn size_or_auto(value: &str) -> Result<Option<u64>> {
    let normalized = value.trim().to_ascii_uppercase();
    if normalized == "AUTO" {
        return Ok(None);
    }
    let units: [(&str, u64); 9] = [
        ("KIB", 1 << 10), ("MIB", 1 << 20), ("GIB", 1 << 30),
        ("KB", 1 << 10), ("MB", 1 << 20), ("GB", 1 << 30),
        ("K", 1 << 10), ("M", 1 << 20), ("G", 1 << 30),
    ];
    let (number, multiplier) = units
        .iter()
        .find_map(|(s, m)| normalized.strip_suffix(s).map(|n| (n.to_string(), *m)))
        .unwrap_or((normalized.clone(), 1));
    let (whole, fraction) = number.split_once('.').unwrap_or((&number, ""));
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let ok = digits(whole) && (fraction.is_empty() || (multiplier > 1 && digits(fraction)));
    let size = if ok {
        // Exact decimal arithmetic, as Python's Decimal: whole * m + floor(fraction * m / 10^len).
        let w: u128 = whole.parse().unwrap_or(u128::MAX / 4);
        let f: u128 = if fraction.is_empty() { 0 } else { fraction.parse().unwrap_or(0) };
        let scale = 10u128.checked_pow(fraction.len() as u32).unwrap_or(u128::MAX);
        w.saturating_mul(multiplier as u128).saturating_add(f.saturating_mul(multiplier as u128) / scale)
    } else {
        0
    };
    ensure!((1..=i64::MAX as u128).contains(&size), "must be 'auto' or a positive byte count such as 32G or 2.5G");
    Ok(Some(size as u64))
}

fn context(value: &str) -> Result<Option<u32>> {
    let n = value.trim().to_ascii_uppercase();
    if n == "AUTO" {
        return Ok(None);
    }
    let (num, mul) = match n.chars().last() {
        Some('K') => (&n[..n.len() - 1], 1024u64),
        Some('M') => (&n[..n.len() - 1], 1_000_000),
        _ => (n.as_str(), 1),
    };
    let tokens = num.parse::<u64>().unwrap_or(0).saturating_mul(mul);
    ensure!((1..=MAX_YARN_CONTEXT_TOKENS as u64).contains(&tokens), "--max-context must be 'auto' or a token count up to 1010000, such as 100K or 1M");
    Ok(Some(tokens as u32))
}

/// Seconds, or with an s, m or h suffix; None unless positive and finite.
fn duration(value: &str) -> Option<f64> {
    let t = value.trim().to_ascii_lowercase();
    let unit = match t.chars().last()? {
        's' => Some(1.0),
        'm' => Some(60.0),
        'h' => Some(3600.0),
        _ => None,
    };
    let seconds = match unit {
        Some(u) => t[..t.len() - 1].parse::<f64>().ok()? * u,
        None => t.parse::<f64>().ok()?,
    };
    (seconds.is_finite() && seconds > 0.0).then_some(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(s: &[&str]) -> Vec<OsString> {
        s.iter().map(OsString::from).collect()
    }
    #[test]
    fn heimdalls_command_line_parses_and_maps_to_the_engines() {
        let o = ServeOptions::parse(&args(&[
            "--model", "unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M", "--port", "8100", "--served-model-name", "qwen3.8-27b",
            "--persistent-cache", "--max-cache-disk", "100G", "--cache-dir", "/d/prefix-cache", "--max-memory", "48G",
        ]))
        .unwrap();
        assert_eq!((o.port, o.max_memory, o.max_cache_disk), (8100, Some(51539607552), Some(107374182400)));
        let c = o.native_command("/e/splash".as_ref(), "/m".as_ref());
        let c: Vec<_> = c.iter().map(|s| s.to_str().unwrap()).collect();
        // The command line Splash 1.4.1's Python built for the same options.
        assert_eq!(c, ["/e/splash", "serve-native", "/m", "auto", "51539607552", "107374182400", "--persistent-cache", "on", "--cache-dir", "/d/prefix-cache"]);
    }
    #[test]
    fn sizes_parse_as_splash_does() {
        assert_eq!(size_or_auto("2.5G").unwrap(), Some(2684354560));
        assert_eq!(size_or_auto("auto").unwrap(), None);
        assert_eq!(size_or_auto("1024").unwrap(), Some(1024));
        assert!(size_or_auto("1.5").is_err() && size_or_auto("0").is_err() && size_or_auto("-1G").is_err());
        assert_eq!(size_or_auto("16gib").unwrap(), Some(16 << 30));
        assert_eq!(context("100K").unwrap(), Some(102400));
        assert_eq!(duration("30m"), Some(1800.0));
        assert!(ServeOptions::parse(&args(&["--model", "a/b", "--cache-dir", "/x"])).is_err());
        assert_eq!(python_float(600.0), "600.0");
    }
}
