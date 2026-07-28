use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub relay_host: String,
    pub relay_port: u16,
    pub relay_username: Option<String>,
    pub relay_password: Option<String>,

    pub tls_enabled: bool,
    pub tls_cert: String,
    pub tls_key: String,

    pub sl_api_url: String,
    pub sl_api_key: Option<String>,

    pub upstream_host: String,
    pub upstream_port: u16,
    pub upstream_username: Option<String>,
    pub upstream_password: Option<String>,
    pub upstream_starttls: bool,

    pub data_timeout: u64,
    pub upstream_timeout: u64,

    pub log_level: String,
}

fn env_bool(key: &str, default: bool) -> bool {
    match env::var(key) {
        Ok(v) => v.to_lowercase() == "true",
        Err(_) => default,
    }
}

fn env_string(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_opt(key: &str) -> Option<String> {
    match env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

fn env_u16(key: &str, default: u16) -> u16 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

impl Config {
    /// Mirrors server.py's module-level env var reads exactly:
    /// same names, same defaults, same "true"/lowercase semantics.
    pub fn from_env() -> Self {
        Config {
            relay_host: env_string("RELAY_HOST", "0.0.0.0"),
            relay_port: env_u16("RELAY_PORT", 8025),
            relay_username: env_opt("RELAY_USERNAME"),
            relay_password: env_opt("RELAY_PASSWORD"),

            tls_enabled: env_bool("TLS_ENABLED", false),
            tls_cert: env_string("TLS_CERT", ""),
            tls_key: env_string("TLS_KEY", ""),

            sl_api_url: env_string("SL_API_URL", "https://app.simplelogin.io"),
            sl_api_key: env_opt("SL_API_KEY"),

            upstream_host: env_string("UPSTREAM_HOST", "smtp.gmail.com"),
            upstream_port: env_u16("UPSTREAM_PORT", 587),
            upstream_username: env_opt("UPSTREAM_USERNAME"),
            upstream_password: env_opt("UPSTREAM_PASSWORD"),
            upstream_starttls: env_bool("UPSTREAM_STARTTLS", true),

            data_timeout: env_u64("DATA_TIMEOUT", 30),
            upstream_timeout: env_u64("UPSTREAM_TIMEOUT", 15),

            log_level: env_string("LOG_LEVEL", "INFO").to_uppercase(),
        }
    }

    /// Mirrors validate_config() in server.py: checks the same five
    /// required vars, and TLS_ENABLED implies TLS_CERT/TLS_KEY.
    /// Returns Err(missing_field_names) on failure -- caller decides
    /// how to report/exit so this stays testable without process::exit.
    pub fn validate(&self) -> Result<(), Vec<&'static str>> {
        let mut missing = Vec::new();
        if self.relay_username.is_none() {
            missing.push("RELAY_USERNAME");
        }
        if self.relay_password.is_none() {
            missing.push("RELAY_PASSWORD");
        }
        if self.sl_api_key.is_none() {
            missing.push("SL_API_KEY");
        }
        if self.upstream_username.is_none() {
            missing.push("UPSTREAM_USERNAME");
        }
        if self.upstream_password.is_none() {
            missing.push("UPSTREAM_PASSWORD");
        }
        if !missing.is_empty() {
            return Err(missing);
        }
        if self.tls_enabled && (self.tls_cert.is_empty() || self.tls_key.is_empty()) {
            return Err(vec!["TLS_CERT_OR_KEY"]);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // env::set_var isn't process-isolated across tests, so serialize them.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_all() {
        for k in [
            "RELAY_HOST",
            "RELAY_PORT",
            "RELAY_USERNAME",
            "RELAY_PASSWORD",
            "TLS_ENABLED",
            "TLS_CERT",
            "TLS_KEY",
            "SL_API_URL",
            "SL_API_KEY",
            "UPSTREAM_HOST",
            "UPSTREAM_PORT",
            "UPSTREAM_USERNAME",
            "UPSTREAM_PASSWORD",
            "UPSTREAM_STARTTLS",
            "DATA_TIMEOUT",
            "UPSTREAM_TIMEOUT",
            "LOG_LEVEL",
        ] {
            env::remove_var(k);
        }
    }

    #[test]
    fn defaults_match_python() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        let c = Config::from_env();
        assert_eq!(c.relay_host, "0.0.0.0");
        assert_eq!(c.relay_port, 8025);
        assert_eq!(c.tls_enabled, false);
        assert_eq!(c.sl_api_url, "https://app.simplelogin.io");
        assert_eq!(c.upstream_host, "smtp.gmail.com");
        assert_eq!(c.upstream_port, 587);
        assert_eq!(c.upstream_starttls, true);
        assert_eq!(c.data_timeout, 30);
        assert_eq!(c.upstream_timeout, 15);
        assert_eq!(c.log_level, "INFO");
    }

    #[test]
    fn missing_required_vars_reported() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        let c = Config::from_env();
        let err = c.validate().unwrap_err();
        assert!(err.contains(&"RELAY_USERNAME"));
        assert!(err.contains(&"RELAY_PASSWORD"));
        assert!(err.contains(&"SL_API_KEY"));
        assert!(err.contains(&"UPSTREAM_USERNAME"));
        assert!(err.contains(&"UPSTREAM_PASSWORD"));
    }

    #[test]
    fn tls_enabled_requires_cert_and_key() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        env::set_var("RELAY_USERNAME", "u");
        env::set_var("RELAY_PASSWORD", "p");
        env::set_var("SL_API_KEY", "k");
        env::set_var("UPSTREAM_USERNAME", "u");
        env::set_var("UPSTREAM_PASSWORD", "p");
        env::set_var("TLS_ENABLED", "true");
        let c = Config::from_env();
        assert!(c.validate().is_err());
        env::set_var("TLS_CERT", "/tmp/cert.pem");
        env::set_var("TLS_KEY", "/tmp/key.pem");
        let c2 = Config::from_env();
        assert!(c2.validate().is_ok());
        clear_all();
    }

    #[test]
    fn valid_config_passes() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        env::set_var("RELAY_USERNAME", "u");
        env::set_var("RELAY_PASSWORD", "p");
        env::set_var("SL_API_KEY", "k");
        env::set_var("UPSTREAM_USERNAME", "u");
        env::set_var("UPSTREAM_PASSWORD", "p");
        let c = Config::from_env();
        assert!(c.validate().is_ok());
        clear_all();
    }
}
