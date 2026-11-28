fn is_valid_numeric_ident(s: &str) -> bool {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if s.len() > 1 && s.starts_with('0') {
        return false;
    }
    true
}

fn is_valid_prerelease_ident(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return false;
    }
    if s.bytes().all(|b| b.is_ascii_digit()) && s.len() > 1 && s.starts_with('0') {
        return false;
    }
    true
}

fn is_valid_build_ident(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn validate_strict_semver(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    let stripped = trimmed.strip_prefix('v').unwrap_or(trimmed);
    if stripped.is_empty() {
        return Err("version string cannot be empty".to_string());
    }

    let (without_build, build) = match stripped.split_once('+') {
        Some((left, right)) => (left, Some(right)),
        None => (stripped, None),
    };

    if let Some(build_meta) = build {
        if !build_meta.split('.').all(is_valid_build_ident) {
            return Err(format!(
                "invalid build metadata in '{raw}'; expected [0-9A-Za-z-]+ segments"
            ));
        }
    }

    let (core, prerelease) = match without_build.split_once('-') {
        Some((left, right)) => (left, Some(right)),
        None => (without_build, None),
    };

    if let Some(pre) = prerelease {
        if !pre.split('.').all(is_valid_prerelease_ident) {
            return Err(format!(
                "invalid prerelease identifier in '{raw}'; numeric segments must not have leading zeros"
            ));
        }
    }

    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 || !parts.iter().all(|p| is_valid_numeric_ident(p)) {
        return Err(format!(
            "invalid STITCH_APP_VERSION '{raw}': must be strict SemVer without leading zeros (e.g. '2027.1.5', not '2027.01.05')"
        ));
    }

    Ok(stripped.to_string())
}

fn main() {
    println!("cargo:rerun-if-env-changed=STITCH_APP_VERSION");

    let pkg_version = std::env::var("CARGO_PKG_VERSION")
        .expect("CARGO_PKG_VERSION must be set by Cargo");

    let resolved = match std::env::var("STITCH_APP_VERSION") {
        Ok(val) if !val.trim().is_empty() => validate_strict_semver(&val)
            .unwrap_or_else(|err| panic!("{err}")),
        _ => validate_strict_semver(&pkg_version)
            .unwrap_or_else(|err| panic!("invalid CARGO_PKG_VERSION: {err}")),
    };

    println!("cargo:rustc-env=STITCH_APP_VERSION={resolved}");
}
