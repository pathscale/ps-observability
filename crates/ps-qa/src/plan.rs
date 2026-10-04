//! A typed, application-owned plan for running real UI checks.
//!
//! Plans name an existing host binary through a declared environment binding,
//! a page URL, the application's profile, and the RON check groups to run.
//! They never contain shell commands. Secret bindings contain environment
//! variable names only; their values are resolved for one session in memory.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::qa::SecretInput;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum EnvironmentKind {
    Host,
    Page,
    Secret,
    Assertion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum AssertionFormat {
    Text,
    SecureWebSocket,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentBinding {
    pub id: String,
    pub name: String,
    pub kind: EnvironmentKind,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub format: Option<AssertionFormat>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckSuite {
    /// RON files/directories jointly form this suite's check manifest.
    pub checks: Vec<PathBuf>,
    /// A check group or one check id.
    pub selector: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestPlan {
    pub id: String,
    /// Application profile used only by this fresh host invocation.
    pub app_profile: PathBuf,
    #[serde(default)]
    pub suites: Vec<CheckSuite>,
    #[serde(default)]
    pub inventory: Option<InventoryPlan>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryPlan {
    /// Check directories/files jointly form one outcome manifest.
    pub checks: Vec<PathBuf>,
    /// Run the strict inventory mode; false is rejected during preflight.
    pub require_outcomes: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPlan {
    pub id: String,
    /// Binding ids in `environment`, not raw process environment names.
    pub username: String,
    pub password: String,
    pub landing: String,
    #[serde(default)]
    pub optional: bool,
    /// Optional sessions may be skipped if their username aliases a protected
    /// session. The username itself is never included in a report.
    #[serde(default)]
    pub skip_on_username_alias: bool,
    /// Explicitly permits multiple sessions to authenticate as this same
    /// account. Every configured session sharing a username must name the
    /// same identity group; unrelated accounts cannot share one group.
    #[serde(default)]
    pub identity_group: Option<String>,
    pub suites: Vec<CheckSuite>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunPlan {
    pub version: u8,
    pub environment: Vec<EnvironmentBinding>,
    /// Binding id whose environment variable contains the host executable.
    pub host: String,
    /// Binding id whose environment variable contains the page URL.
    pub page: String,
    pub login: Option<CheckSuite>,
    #[serde(default)]
    pub guests: Vec<GuestPlan>,
    pub sessions: Vec<SessionPlan>,
    #[serde(default = "default_startup_timeout_secs")]
    pub startup_timeout_secs: u64,
}

fn default_startup_timeout_secs() -> u64 {
    45
}

impl RunPlan {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("could not read run plan {}: {error}", path.display()))?;
        let mut plan: Self = ron::from_str(&text)
            .map_err(|error| format!("could not parse run plan {}: {error}", path.display()))?;
        plan.validate(path)?;
        let base = path
            .canonicalize()
            .map_err(|error| format!("could not resolve run plan {}: {error}", path.display()))?;
        let base = base.parent().unwrap_or_else(|| Path::new("."));
        if let Some(login) = &mut plan.login {
            resolve_suite_paths(base, login)?;
        }
        for guest in &mut plan.guests {
            resolve_profile_path(base, &mut guest.app_profile)?;
            for suite in &mut guest.suites {
                resolve_suite_paths(base, suite)?;
            }
            if let Some(inventory) = &mut guest.inventory {
                for checks in &mut inventory.checks {
                    resolve_checks_path(base, checks)?;
                }
            }
        }
        for session in &mut plan.sessions {
            for suite in &mut session.suites {
                resolve_suite_paths(base, suite)?;
            }
        }
        Ok(plan)
    }

    fn validate(&self, path: &Path) -> Result<(), String> {
        if self.version != 1 {
            return Err(format!(
                "run plan {} has unsupported version {}; expected 1",
                path.display(),
                self.version
            ));
        }
        if self.startup_timeout_secs == 0 || self.startup_timeout_secs > 300 {
            return Err("run plan startup_timeout_secs must be between 1 and 300".into());
        }

        let mut bindings = HashMap::new();
        let mut names = std::collections::HashSet::new();
        let mut host_count = 0;
        let mut page_count = 0;
        for binding in &self.environment {
            if !valid_binding_id(&binding.id) {
                return Err(format!("invalid environment binding id {:?}", binding.id));
            }
            if !valid_environment_name(&binding.name) {
                return Err(format!(
                    "invalid environment variable name {:?}",
                    binding.name
                ));
            }
            if bindings.insert(binding.id.as_str(), binding).is_some() {
                return Err(format!("duplicate environment binding id {:?}", binding.id));
            }
            if !names.insert(binding.name.as_str()) {
                return Err(format!(
                    "duplicate environment variable name {:?}",
                    binding.name
                ));
            }
            match binding.kind {
                EnvironmentKind::Host => {
                    host_count += 1;
                    if binding.name != "CHUZZ_HEADLESS_BIN"
                        || binding.default.is_some()
                        || binding.format.is_some()
                    {
                        return Err(
                            "Host must bind CHUZZ_HEADLESS_BIN and cannot declare a default".into(),
                        );
                    }
                }
                EnvironmentKind::Page => {
                    page_count += 1;
                    if binding.name != "QA_SITE_URL" {
                        return Err("Page must bind QA_SITE_URL".into());
                    }
                    if binding.format.is_some() {
                        return Err(format!(
                            "Page environment binding {:?} cannot declare a value format",
                            binding.id
                        ));
                    }
                    if binding
                        .default
                        .as_deref()
                        .is_some_and(|url| !valid_page_location(url))
                    {
                        return Err(format!(
                            "invalid default page location for {:?}",
                            binding.id
                        ));
                    }
                }
                EnvironmentKind::Secret => {
                    if binding.default.is_some() || binding.format.is_some() {
                        return Err(format!(
                            "Secret environment binding {:?} cannot declare defaults or formats",
                            binding.id
                        ));
                    }
                }
                EnvironmentKind::Assertion => {
                    let Some(format) = binding.format else {
                        return Err(format!(
                            "Assertion environment binding {:?} must declare a format",
                            binding.id
                        ));
                    };
                    if binding
                        .default
                        .as_deref()
                        .is_some_and(|value| !valid_assertion_value(format, value))
                    {
                        return Err(format!(
                            "invalid default assertion value for {:?}",
                            binding.id
                        ));
                    }
                }
            }
        }
        if host_count != 1 || page_count != 1 {
            return Err("run plan must declare exactly one Host and one Page binding".into());
        }
        require_binding(&bindings, &self.host, EnvironmentKind::Host)?;
        require_binding(&bindings, &self.page, EnvironmentKind::Page)?;
        if let Some(login) = &self.login {
            validate_suite(login)?;
        }

        if self.sessions.is_empty() && self.guests.is_empty() {
            return Err("run plan must declare guest invocations or signed-in sessions".into());
        }
        let mut run_ids = std::collections::HashSet::new();
        for guest in &self.guests {
            if !valid_binding_id(&guest.id) || !run_ids.insert(guest.id.as_str()) {
                return Err(format!(
                    "invalid or duplicate guest invocation id {:?}",
                    guest.id
                ));
            }
            if guest.app_profile.as_os_str().is_empty() {
                return Err(format!(
                    "guest invocation {:?} has an empty app profile",
                    guest.id
                ));
            }
            let has_suites = !guest.suites.is_empty();
            let has_inventory = guest.inventory.is_some();
            if has_suites == has_inventory {
                return Err(format!(
                    "guest invocation {:?} must declare exactly one of suites or inventory",
                    guest.id
                ));
            }
            if has_suites {
                validate_suites(&guest.suites)?;
            }
            if let Some(inventory) = &guest.inventory {
                if inventory.checks.is_empty() {
                    return Err(format!(
                        "guest inventory {:?} must declare at least one check path",
                        guest.id
                    ));
                }
                if !inventory.require_outcomes {
                    return Err(format!(
                        "guest inventory {:?} must require named outcomes",
                        guest.id
                    ));
                }
            }
        }
        let mut used_secret_bindings = std::collections::HashSet::new();
        if !self.sessions.is_empty() && self.login.is_none() {
            return Err("run plan with signed-in sessions must declare a login suite".into());
        }
        if self.sessions.is_empty() && self.login.is_some() {
            return Err("run plan without signed-in sessions cannot declare a login suite".into());
        }
        for session in &self.sessions {
            if !valid_binding_id(&session.id) || !run_ids.insert(session.id.as_str()) {
                return Err(format!("invalid or duplicate session id {:?}", session.id));
            }
            if session.landing.trim().is_empty() {
                return Err(format!(
                    "session {:?} has an empty landing selector",
                    session.id
                ));
            }
            if session.skip_on_username_alias && !session.optional {
                return Err(format!(
                    "session {:?} can skip on username alias only when optional",
                    session.id
                ));
            }
            if let Some(group) = session.identity_group.as_deref()
                && !valid_binding_id(group)
            {
                return Err(format!(
                    "session {:?} has an invalid identity group",
                    session.id
                ));
            }
            let username = require_binding(&bindings, &session.username, EnvironmentKind::Secret)?;
            let password = require_binding(&bindings, &session.password, EnvironmentKind::Secret)?;
            if !username.name.ends_with("_USERNAME") || !password.name.ends_with("_PASSWORD") {
                return Err(format!(
                    "session {:?} must bind username and password variables by their suffix",
                    session.id
                ));
            }
            used_secret_bindings.insert(username.id.as_str());
            used_secret_bindings.insert(password.id.as_str());
            if username.id == password.id {
                return Err(format!(
                    "session {:?} must use separate username and password bindings",
                    session.id
                ));
            }
            validate_suites(&session.suites)?;
        }
        if self
            .environment
            .iter()
            .filter(|binding| binding.kind == EnvironmentKind::Secret)
            .any(|binding| !used_secret_bindings.contains(binding.id.as_str()))
        {
            return Err("run plan declares an unused Secret environment binding".into());
        }
        Ok(())
    }

    pub fn resolve(&self) -> Result<ResolvedPlan, String> {
        let by_id: HashMap<&str, &EnvironmentBinding> = self
            .environment
            .iter()
            .map(|binding| (binding.id.as_str(), binding))
            .collect();
        let host_binding = require_binding(&by_id, &self.host, EnvironmentKind::Host)?;
        let page_binding = require_binding(&by_id, &self.page, EnvironmentKind::Page)?;
        let host = required_environment(&host_binding.name)?;
        if !Path::new(&host).is_file() {
            return Err("configured headless host is not a file".into());
        }
        let page = optional_environment(&page_binding.name)?
            .or_else(|| page_binding.default.clone())
            .ok_or_else(|| {
                format!(
                    "required environment variable {} is not set",
                    page_binding.name
                )
            })?;
        if !valid_page_location(&page) {
            return Err(format!(
                "environment variable {} is not a valid page URL or local build path",
                page_binding.name
            ));
        }

        let mut assertion_values = HashMap::new();
        for binding in self
            .environment
            .iter()
            .filter(|binding| binding.kind == EnvironmentKind::Assertion)
        {
            let format = binding
                .format
                .ok_or_else(|| format!("Assertion binding {:?} has no format", binding.id))?;
            let value = optional_environment(&binding.name)?
                .or_else(|| binding.default.clone())
                .ok_or_else(|| {
                    format!("required environment variable {} is not set", binding.name)
                })?;
            if !valid_assertion_value(format, &value) {
                return Err(format!(
                    "environment variable {} is not a valid assertion value",
                    binding.name
                ));
            }
            assertion_values.insert(binding.id.clone(), value);
        }

        let mut sessions = Vec::with_capacity(self.sessions.len());
        for session in &self.sessions {
            let username_binding =
                require_binding(&by_id, &session.username, EnvironmentKind::Secret)?;
            let password_binding =
                require_binding(&by_id, &session.password, EnvironmentKind::Secret)?;
            let username = optional_environment(&username_binding.name)?;
            let password = optional_environment(&password_binding.name)?;
            let credentials = match (username, password) {
                (Some(username), Some(password)) => Some(SessionSecrets { username, password }),
                (None, None) if session.optional => None,
                (None, None) => {
                    return Err(format!(
                        "missing required environment variables: {}, {}",
                        username_binding.name, password_binding.name
                    ));
                }
                (None, Some(_)) => {
                    return Err(format!(
                        "missing required environment variable {}",
                        username_binding.name
                    ));
                }
                (Some(_), None) => {
                    return Err(format!(
                        "missing required environment variable {}",
                        password_binding.name
                    ));
                }
            };
            sessions.push(ResolvedSession {
                id: session.id.clone(),
                landing: session.landing.clone(),
                skip_on_username_alias: session.skip_on_username_alias,
                identity_group: session.identity_group.clone(),
                suites: session.suites.clone(),
                credentials,
                skip_reason: None,
            });
        }

        let mut usernames: HashMap<String, Vec<usize>> = HashMap::new();
        let mut identity_group_sessions: HashMap<String, usize> = HashMap::new();
        for (index, session) in sessions.iter().enumerate() {
            if let Some(credentials) = &session.credentials {
                usernames
                    .entry(credentials.username.clone())
                    .or_default()
                    .push(index);
                if let Some(group) = session.identity_group.as_deref()
                    && let Some(previous_index) =
                        identity_group_sessions.insert(group.to_owned(), index)
                {
                    let previous = sessions[previous_index]
                        .credentials
                        .as_ref()
                        .expect("only configured sessions are indexed");
                    if previous.username.as_str() != credentials.username.as_str()
                        || previous.password.as_str() != credentials.password.as_str()
                    {
                        return Err("an identity group cannot contain different accounts".into());
                    }
                }
            }
        }
        for indexes in usernames.values().filter(|indexes| indexes.len() > 1) {
            let protected = indexes
                .iter()
                .filter(|index| !sessions[**index].skip_on_username_alias)
                .count();
            let explicit_shared_identity = indexes
                .iter()
                .filter(|index| !sessions[**index].skip_on_username_alias)
                .map(|index| sessions[*index].identity_group.as_deref())
                .collect::<std::collections::HashSet<_>>();
            let alias_is_explicit = explicit_shared_identity.len() == 1
                && explicit_shared_identity.iter().all(|group| group.is_some());
            if protected > 1 && !alias_is_explicit {
                return Err("each protected session requires a distinct account".into());
            }
            for index in indexes {
                if sessions[*index].skip_on_username_alias {
                    sessions[*index].skip_reason =
                        Some("optional session shares an account identity with another session");
                }
            }
        }

        for session in &mut sessions {
            if session.credentials.is_none() && session.skip_reason.is_none() {
                session.skip_reason = Some("optional credentials are not configured");
            }
        }

        Ok(ResolvedPlan {
            host: PathBuf::from(host),
            page,
            login: self.login.clone(),
            guests: self.guests.clone(),
            sessions,
            assertion_values,
            startup_timeout_secs: self.startup_timeout_secs,
        })
    }
}

fn validate_suites(suites: &[CheckSuite]) -> Result<(), String> {
    if suites.is_empty() {
        return Err("a run-plan suite list cannot be empty".into());
    }
    for suite in suites {
        validate_suite(suite)?;
    }
    Ok(())
}

fn validate_suite(suite: &CheckSuite) -> Result<(), String> {
    if suite.selector.trim().is_empty() {
        return Err("a run-plan check selector cannot be empty".into());
    }
    if suite.checks.is_empty() || suite.checks.iter().any(|path| path.as_os_str().is_empty()) {
        return Err("a run-plan check path cannot be empty".into());
    }
    Ok(())
}

fn resolve_suite_paths(base: &Path, suite: &mut CheckSuite) -> Result<(), String> {
    for checks in &mut suite.checks {
        resolve_checks_path(base, checks)?;
    }
    Ok(())
}

fn resolve_profile_path(base: &Path, profile: &mut PathBuf) -> Result<(), String> {
    if profile.is_absolute()
        || profile
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!(
            "run-plan profile path must stay under its plan directory: {}",
            profile.display()
        ));
    }
    *profile = base.join(&*profile);
    let resolved = profile
        .canonicalize()
        .map_err(|_| format!("run-plan profile not found at {}", profile.display()))?;
    if !resolved.starts_with(base)
        || !resolved.is_file()
        || !resolved
            .extension()
            .is_some_and(|extension| extension == "ron")
    {
        return Err("run-plan app profile must be a .ron file under its plan directory".into());
    }
    *profile = resolved;
    Ok(())
}

fn resolve_checks_path(base: &Path, checks: &mut PathBuf) -> Result<(), String> {
    if checks.is_absolute()
        || checks
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!(
            "run-plan check path must stay under its plan directory: {}",
            checks.display()
        ));
    }
    *checks = base.join(&*checks);
    let resolved = checks
        .canonicalize()
        .map_err(|_| format!("run-plan checks not found at {}", checks.display()))?;
    if !resolved.starts_with(base)
        || (!resolved.is_file() && !resolved.is_dir())
        || (resolved.is_file()
            && !resolved
                .extension()
                .is_some_and(|extension| extension == "ron"))
    {
        return Err(
            "run-plan check paths must be .ron files or directories under its plan directory"
                .into(),
        );
    }
    if resolved.is_dir() {
        let entries = std::fs::read_dir(&resolved)
            .map_err(|_| "could not read run-plan checks directory".to_owned())?;
        for entry in entries {
            let entry = entry.map_err(|_| "could not read run-plan checks directory".to_owned())?;
            let candidate = entry.path();
            if !candidate
                .extension()
                .is_some_and(|extension| extension == "ron")
                || candidate
                    .file_name()
                    .is_some_and(|name| name == "ps-qa.ron")
            {
                continue;
            }
            let resolved_file = candidate
                .canonicalize()
                .map_err(|_| "run-plan check files must stay under its plan directory")?;
            if !resolved_file.starts_with(base) || !resolved_file.is_file() {
                return Err("run-plan check files must stay under its plan directory".into());
            }
        }
    }
    *checks = resolved;
    Ok(())
}

fn require_binding<'a>(
    bindings: &'a HashMap<&str, &'a EnvironmentBinding>,
    id: &str,
    kind: EnvironmentKind,
) -> Result<&'a EnvironmentBinding, String> {
    let Some(binding) = bindings.get(id).copied() else {
        return Err(format!("unknown run-plan environment binding {id:?}"));
    };
    if binding.kind != kind {
        return Err(format!(
            "run-plan environment binding {id:?} has the wrong kind"
        ));
    }
    Ok(binding)
}

fn valid_binding_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
}

fn valid_environment_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_uppercase() || first == '_')
        && chars.all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
}

fn valid_page_url(url: &str) -> bool {
    let url = url.trim();
    let Some(authority) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return false;
    };
    let host = authority.split(['/', '?', '#']).next().unwrap_or_default();
    !host.is_empty()
        && !host.contains('@')
        && !url.contains('?')
        && !url.contains('#')
        && !url.chars().any(char::is_whitespace)
}

fn valid_page_location(location: &str) -> bool {
    if valid_page_url(location) {
        return true;
    }
    let path = Path::new(location);
    !location.trim().is_empty()
        && !location.chars().any(char::is_whitespace)
        && !path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
}

fn valid_assertion_value(format: AssertionFormat, value: &str) -> bool {
    if value.trim().is_empty() || value.len() > 2048 || value.chars().any(char::is_control) {
        return false;
    }
    match format {
        AssertionFormat::Text => true,
        AssertionFormat::SecureWebSocket => {
            let Some(authority) = value.strip_prefix("wss://") else {
                return false;
            };
            let host = authority.split(['/', '?', '#']).next().unwrap_or_default();
            !host.is_empty()
                && !host.contains('@')
                && !value.contains('?')
                && !value.contains('#')
                && !value.chars().any(char::is_whitespace)
        }
    }
}

fn optional_environment(name: &str) -> Result<Option<String>, String> {
    match std::env::var_os(name) {
        None => Ok(None),
        Some(value) => os_string(value, name).map(|value| {
            if value.trim().is_empty() {
                None
            } else {
                Some(value)
            }
        }),
    }
}

fn required_environment(name: &str) -> Result<String, String> {
    optional_environment(name)?
        .ok_or_else(|| format!("required environment variable {name} is not set"))
}

fn os_string(value: OsString, name: &str) -> Result<String, String> {
    value
        .into_string()
        .map_err(|_| format!("environment variable {name} is not valid UTF-8"))
}

pub struct SessionSecrets {
    username: String,
    password: String,
}

impl SessionSecrets {
    pub fn value(&self, input: SecretInput) -> &str {
        match input {
            SecretInput::Username => &self.username,
            SecretInput::Password => &self.password,
        }
    }

    pub fn redact(&self, text: &str) -> String {
        let mut redacted = text.to_owned();
        for secret in [&self.username, &self.password] {
            if !secret.is_empty() {
                redacted = redacted.replace(secret, "[redacted]");
                if let Ok(encoded) = serde_json::to_string(secret)
                    && let Some(escaped) = encoded.get(1..encoded.len().saturating_sub(1))
                    && !escaped.is_empty()
                {
                    redacted = redacted.replace(escaped, "[redacted]");
                }
            }
        }
        redacted
    }
}

pub struct ResolvedSession {
    pub id: String,
    pub landing: String,
    pub suites: Vec<CheckSuite>,
    pub identity_group: Option<String>,
    pub credentials: Option<SessionSecrets>,
    pub skip_reason: Option<&'static str>,
    skip_on_username_alias: bool,
}

pub struct ResolvedPlan {
    pub host: PathBuf,
    pub page: String,
    pub login: Option<CheckSuite>,
    pub guests: Vec<GuestPlan>,
    pub assertion_values: HashMap<String, String>,
    pub sessions: Vec<ResolvedSession>,
    pub startup_timeout_secs: u64,
}
