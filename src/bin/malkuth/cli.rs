//! Command-line interface for the `malkuth` watchdog binary.
//!
//! `malkuth [--watch PATH]... [--proxy PUBLIC:LO-HI] [--pod-count N] -- <cmd>…`
//!
//! Wraps any program (even one that does not use the malkuth library) with:
//! - a pod pool of N parallel instances,
//! - a file watcher that triggers rolling restarts,
//! - an L4 TCP reverse proxy with sticky (client-IP) routing.

use std::path::PathBuf;

use clap::Parser;

/// `malkuth` — watchdog-style process supervisor + sticky reverse proxy.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "malkuth",
    version,
    about = "Wrap any program with a supervised pod pool, file watching and a sticky reverse proxy."
)]
pub struct Args {
    /// Paths to watch for changes (repeatable). A change triggers a rolling
    /// restart of the pods, one at a time.
    #[arg(long = "watch", value_name = "PATH")]
    pub watch: Vec<PathBuf>,

    /// Reverse-proxy spec `PUBLIC:LO-HI`, e.g. `3000:3000-3999`. The CLI listens
    /// on PUBLIC and forwards to the pods' backend ports, which are assigned
    /// from the inclusive range LO..=HI (skipping PUBLIC itself). Sticky by
    /// client IP via consistent hashing.
    #[arg(long = "proxy", value_name = "PUBLIC:LO-HI")]
    pub proxy: Option<String>,

    /// Number of parallel pod instances to run (load balancing / gray release).
    #[arg(long = "pod-count", default_value = "1")]
    pub pod_count: usize,

    /// Host the proxy binds and that pods report as their backend address.
    #[arg(long = "host", default_value = "127.0.0.1")]
    pub host: String,

    /// Environment variable through which the assigned backend port is handed
    /// to the wrapped program (the program is expected to listen on it).
    #[arg(long = "port-env", default_value = "PORT")]
    pub port_env: String,

    /// Seconds a sticky client→backend mapping is preserved even if the ring
    /// shifts (best-effort; a dead backend is re-mapped immediately).
    #[arg(long = "sticky-ttl-secs", default_value = "300")]
    pub sticky_ttl_secs: u64,

    /// Graceful drain budget (seconds) given to a pod before SIGKILL on restart.
    #[arg(long = "drain-secs", default_value = "8")]
    pub drain_secs: u64,

    /// The command to run (everything after `--`), e.g. `-- cargo run`.
    #[arg(last = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

/// Parsed `--proxy` spec.
#[derive(Debug, Clone, Copy)]
pub struct ProxySpec {
    /// Public-facing port the proxy listens on.
    pub public_port: u16,
    /// Inclusive lower bound of the backend port range.
    pub range_lo: u16,
    /// Inclusive upper bound of the backend port range.
    pub range_hi: u16,
}

impl ProxySpec {
    pub fn parse(s: &str) -> Result<Self, String> {
        let (pub_s, range_s) = s
            .split_once(':')
            .ok_or_else(|| format!("--proxy must look like PUBLIC:LO-HI (got `{s}`)"))?;
        let public_port: u16 = pub_s
            .parse()
            .map_err(|_| format!("invalid public port `{pub_s}`"))?;
        let (lo_s, hi_s) = range_s
            .split_once('-')
            .ok_or_else(|| format!("backend range must look like LO-HI (got `{range_s}`)"))?;
        let range_lo: u16 = lo_s
            .parse()
            .map_err(|_| format!("invalid range lo `{lo_s}`"))?;
        let range_hi: u16 = hi_s
            .parse()
            .map_err(|_| format!("invalid range hi `{hi_s}`"))?;
        if range_hi < range_lo {
            return Err(format!("range hi {range_hi} < lo {range_lo}"));
        }
        Ok(Self {
            public_port,
            range_lo,
            range_hi,
        })
    }

    /// Iterate candidate backend ports, skipping the public port.
    pub fn backend_ports(&self) -> impl Iterator<Item = u16> {
        (self.range_lo..=self.range_hi).filter(move |p| *p != self.public_port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_proxy_spec() {
        let s = ProxySpec::parse("3000:3000-3999").unwrap();
        assert_eq!(s.public_port, 3000);
        assert_eq!(s.range_lo, 3000);
        assert_eq!(s.range_hi, 3999);
        let ports: Vec<u16> = s.backend_ports().take(5).collect();
        assert_eq!(ports, vec![3001, 3002, 3003, 3004, 3005]);
    }

    #[test]
    fn parse_rejects_bad() {
        assert!(ProxySpec::parse("3000").is_err());
        assert!(ProxySpec::parse("3000:4000-3999").is_err());
    }
}
