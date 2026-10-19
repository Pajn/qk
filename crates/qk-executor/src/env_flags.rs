//! What the environment says about where qk runs: in CI, and whether colour
//! is forced, read as Nx reads them.

/// Whether this runs in CI, as Nx's `isCI` decides it: `CI` set to anything
/// but `false` (`0` included), or a variable a known CI provider sets.
pub fn is_ci() -> bool {
    is_ci_with(|name| std::env::var(name).ok())
}

/// [`is_ci`], reading variables through `variable`.
pub fn is_ci_with(variable: impl Fn(&str) -> Option<String>) -> bool {
    let ci = variable("CI");
    if ci.as_deref() == Some("false") {
        return false;
    }
    let set = |name: &str| variable(name).is_some_and(|value| !value.is_empty());
    let is_true = |name: &str| variable(name).as_deref() == Some("true");
    ci.is_some_and(|value| !value.is_empty())
        || [
            "TF_BUILD",
            "GITHUB_ACTIONS",
            "BUILDKITE",
            "CIRCLECI",
            "CIRRUS_CI",
            "TRAVIS",
        ]
        .into_iter()
        .any(is_true)
        || [
            "bamboo.buildKey",
            "bamboo_buildKey",
            "CODEBUILD_BUILD_ID",
            "GITLAB_CI",
            "HEROKU_TEST_RUN_ID",
            "BUILD_ID",
            "BUILD_NUMBER",
            "BUILD_BUILDID",
            "TEAMCITY_VERSION",
            "JENKINS_URL",
            "HUDSON_URL",
        ]
        .into_iter()
        .any(set)
}

/// What `FORCE_COLOR` asks of qk's own output: colour when it is set, none
/// when it is `0` or `false`, and nothing when it is unset. Tasks receive
/// the variable as it is.
pub fn force_colour() -> Option<bool> {
    let value = std::env::var_os("FORCE_COLOR")?;
    Some(value != "0" && value != "false")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ci(variables: &[(&str, &str)]) -> bool {
        is_ci_with(|name| {
            variables
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        })
    }

    #[test]
    fn reads_ci_as_nx_does() {
        assert!(ci(&[("CI", "0")]));
        assert!(ci(&[("CI", "1")]));
        assert!(!ci(&[("CI", "false")]));
        assert!(!ci(&[("CI", "false"), ("GITHUB_ACTIONS", "true")]));
        assert!(!ci(&[]));
        assert!(!ci(&[("CI", "")]));
        assert!(ci(&[("GITHUB_ACTIONS", "true")]));
        assert!(!ci(&[("GITHUB_ACTIONS", "1")]));
        assert!(ci(&[("GITLAB_CI", "1")]));
        assert!(ci(&[("JENKINS_URL", "http://ci")]));
    }
}
