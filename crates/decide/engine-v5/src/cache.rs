//! MLX's buffer cache, capped. MLX keeps every freed GPU buffer for reuse, and by default the cache may grow to its
//! memory limit (about 1.5x the GPU's recommended working set), so in a long-lived process (Heimdall) the buffers freed
//! by the load-time weight transforms (fuse's concatenations, the SwiGLU interleave) and every decision's activations
//! stay resident as GPU memory after they are dead.
//!
//! The engine caps the cache at `DEFAULT_LIMIT` when it loads, clears it after the load's transforms, and clears it
//! again after every request (`ffi`), so the process holds the weights plus at most the cap between decisions.
//! `D1E_CACHE_LIMIT_MB` overrides the cap (a number of MB, `0` keeps no cache); `off` restores MLX's own behaviour
//! (no cap, never cleared), for before/after measurement.

use anyhow::{anyhow, Result};

/// The cap: 64 MB. Measured 30 Oct (lab/memory-cache/README.md, 30 decisions of 508 to 3,165 tokens, M5 Max): caps
/// of 0, 64 and 256 MB gave the same median decision time (87-89 ms) and identical outputs; what stays resident after
/// the per-request clear (buffers whose GPU work finishes after it) grows with the cap (3.48 / 3.56 / 3.63-3.71 GB
/// IOAccelerator), and 64 MB keeps a little reuse inside a forward without holding more.
pub const DEFAULT_LIMIT: usize = 64 << 20;

/// The cache policy from D1E_CACHE_LIMIT_MB: `Some(bytes)`, or `None` for MLX's own (uncapped, never cleared).
pub fn limit_from(value: Option<&str>) -> Result<Option<usize>> {
    match value.map(str::trim) {
        None | Some("") => Ok(Some(DEFAULT_LIMIT)),
        Some("off") => Ok(None),
        Some(v) => v
            .parse::<usize>()
            .map(|mb| Some(mb << 20))
            .map_err(|_| anyhow!("D1E_CACHE_LIMIT_MB must be a number of MB or \"off\", not {v:?}")),
    }
}

/// The cache cap during one forward of a quantized model whose weights are dequantized where used (qdeq): each
/// layer's dequantized weights (the largest, the fused w1/w3, 63 MB in BF16) are freed into the cache and reused by
/// the next layer, instead of being fresh allocations that the system zero-fills. Restored and cleared after the
/// forward, so the resident policy between decisions is unchanged. D1E_FORWARD_CACHE_MB overrides (0 turns it off).
pub const FORWARD_LIMIT: usize = 256 << 20;

pub fn forward_limit() -> Option<usize> {
    match std::env::var("D1E_FORWARD_CACHE_MB").ok().map(|v| v.trim().to_string()).as_deref() {
        None | Some("") => Some(FORWARD_LIMIT),
        Some(v) => v.parse::<usize>().ok().filter(|mb| *mb > 0).map(|mb| mb << 20),
    }
}

/// Runs `f` with the cache cap raised to `cap` (when the policy is capped and `cap` is larger), then restores the cap
/// and clears the cache.
pub fn with_forward_limit<T>(cap: Option<usize>, f: impl FnOnce() -> T) -> T {
    // a cap of 0 (no cache) set on purpose stays as it is
    let base = match (limit(), cap) {
        (Ok(Some(base)), Some(cap)) if cap > base && base > 0 => base,
        _ => return f(),
    };
    /// restores the cap and clears the cache however `f` ends (a panic included)
    struct Restore(usize);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = mlx_rs::memory::set_cache_limit(self.0);
            let _ = mlx_rs::memory::clear_cache();
        }
    }
    let _restore = Restore(base);
    let _ = mlx_rs::memory::set_cache_limit(cap.unwrap());
    f()
}

pub fn limit() -> Result<Option<usize>> {
    limit_from(std::env::var("D1E_CACHE_LIMIT_MB").ok().as_deref())
}

/// Sets MLX's process-wide cache limit to the policy's cap (nothing under `off`). Returns the cap set.
pub fn apply() -> Result<Option<usize>> {
    let cap = limit()?;
    if let Some(bytes) = cap {
        mlx_rs::memory::set_cache_limit(bytes).map_err(|e| anyhow!("set_cache_limit: {e}"))?;
    }
    Ok(cap)
}

/// Returns the cached buffers to the system, unless the policy is `off`.
pub fn release() {
    if matches!(limit(), Ok(Some(_))) {
        let _ = mlx_rs::memory::clear_cache();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_from_the_environment_value() {
        assert_eq!(limit_from(None).unwrap(), Some(DEFAULT_LIMIT));
        assert_eq!(limit_from(Some("")).unwrap(), Some(DEFAULT_LIMIT));
        assert_eq!(limit_from(Some("64")).unwrap(), Some(64 << 20));
        assert_eq!(limit_from(Some("0")).unwrap(), Some(0));
        assert_eq!(limit_from(Some("off")).unwrap(), None);
        assert!(limit_from(Some("lots")).is_err());
        assert!(DEFAULT_LIMIT <= 256 << 20, "the cap stays small");
    }

    /// The cap reaches MLX: set_cache_limit returns the previous limit, so a second call reads back the first.
    /// No model, no GPU work (MLX's allocator only). The only test here that touches MLX's process-wide state or
    /// the environment; a second one would race it.
    #[test]
    fn apply_sets_mlx_cache_limit() {
        std::env::remove_var("D1E_CACHE_LIMIT_MB");
        assert_eq!(apply().unwrap(), Some(DEFAULT_LIMIT));
        let previous = mlx_rs::memory::set_cache_limit(DEFAULT_LIMIT).unwrap();
        assert_eq!(previous, DEFAULT_LIMIT);
        release();
        assert_eq!(mlx_rs::memory::cache_memory().unwrap(), 0);
    }
}
