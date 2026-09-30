//! Environment configuration: import, change, inspect, discover.
//!
//! See [`crate::configuration`] for the model. The values live where they always
//! have (`EnvironmentRecord.config`); this module keeps what is *known about*
//! them (`EnvironmentRecord.configuration`: sensitivity, source, generation)
//! true through every change, shows them without the values a caller may not
//! see, and adds the operations that use them.
//!
//! * **One writer.** Every path that changes configuration goes through
//!   `change_environment_with`, which calls [`settle_configuration`], so the
//!   metadata and the generation cannot drift from the values.
//! * **Atomic.** An import parses and validates every file, plans every
//!   variable, and only then commits one change (one new generation). A parse
//!   error, an invalid name, or a lost race applies nothing.
//! * **Values stay out of evidence.** Events carry names, sources, and the
//!   generation, never values; errors never echo a line; views omit sensitive
//!   values. A process records the generation it started with.
//! * **Nothing goes into the workspace.** An import does not copy the file
//!   anywhere; the values reach a process's environment when it is started.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use compute_core::ConfigurationVariable;
use compute_state::EnvironmentRecord;

use super::Daemon;
use super::computers::script;
use crate::EnvironmentError;
use crate::configuration::*;
use crate::model::validate_env;

/// Whether a variable's value may not be shown: unless recorded public, it may
/// not. A record that predates the metadata has every variable sensitive.
pub(crate) fn is_sensitive(record: &EnvironmentRecord, name: &str) -> bool {
    record
        .configuration
        .as_ref()
        .and_then(|state| state.variables.get(name))
        .is_none_or(|variable| variable.sensitive)
}

/// The configuration values a caller may see: the public ones.
pub(crate) fn visible_config(record: &EnvironmentRecord) -> BTreeMap<String, String> {
    record
        .config
        .iter()
        .filter(|(name, _)| !is_sensitive(record, name))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// The configuration as a caller sees it: every variable, and only the values
/// that are public.
pub(crate) fn configuration_view(name: &str, record: &EnvironmentRecord) -> ConfigurationView {
    let generation = record
        .configuration
        .as_ref()
        .map_or(0, |state| state.generation);
    ConfigurationView {
        environment: name.to_owned(),
        generation,
        variables: record
            .config
            .iter()
            .map(|(variable, value)| {
                let sensitive = is_sensitive(record, variable);
                ConfigurationInput {
                    name: variable.clone(),
                    configured: true,
                    sensitive,
                    source: record
                        .configuration
                        .as_ref()
                        .and_then(|state| state.variables.get(variable))
                        .map_or_else(|| "declared".to_owned(), |v| v.source.clone()),
                    value: (!sensitive).then(|| value.clone()),
                }
            })
            .collect(),
    }
}

/// Keep `configuration` true after `after.config` (and possibly its metadata)
/// was changed from `before`: every variable has a treatment, none is left for a
/// variable that is gone, and the generation moves exactly when a value, a
/// variable, or a treatment did. A variable that already existed with no
/// recorded treatment is sensitive; a new one is public only if its name says so.
pub(crate) fn settle_configuration(
    before: Option<&EnvironmentRecord>,
    after: &mut EnvironmentRecord,
    generation: u64,
) {
    let previous = before.and_then(|record| record.configuration.clone());
    let mut state = after
        .configuration
        .clone()
        .or_else(|| previous.clone())
        .unwrap_or_default();
    state
        .variables
        .retain(|name, _| after.config.contains_key(name));
    for name in after.config.keys() {
        if state.variables.contains_key(name) {
            continue;
        }
        let existed = before.is_some_and(|record| record.config.contains_key(name));
        state.variables.insert(
            name.clone(),
            ConfigurationVariable {
                sensitive: existed || !is_public(name),
                source: "declared".into(),
            },
        );
    }
    let changed = before.is_none_or(|record| record.config != after.config)
        || previous.as_ref().map(|state| &state.variables) != Some(&state.variables)
            && !(previous.is_none() && state.variables.is_empty());
    state.generation = if changed {
        generation
    } else {
        previous.as_ref().map_or(0, |state| state.generation)
    };
    after.configuration = (!(state.variables.is_empty() && previous.is_none())).then_some(state);
}

/// A caller's replacement of the configuration it could see: what it did not
/// see (a sensitive variable) is kept unless it is removed by name. So a client
/// that read the masked view and writes it back cannot erase what it never had.
pub(crate) fn replace_visible_config(
    before: &EnvironmentRecord,
    update: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut config = update.clone();
    for (name, value) in &before.config {
        if is_sensitive(before, name) && !config.contains_key(name) {
            config.insert(name.clone(), value.clone());
        }
    }
    config
}

impl Daemon {
    /// The environment's configuration, without values a caller may not see.
    pub async fn configuration(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
    ) -> Result<ConfigurationView, EnvironmentError> {
        let record = self.owned_environment(environment, operator).await?;
        Ok(configuration_view(&record.value.name, &record.value))
    }

    /// Import `.env` files: parse and validate all of them, plan every
    /// variable, then apply one new configuration generation, or nothing.
    pub async fn import_configuration(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        request: ConfigImportRequest,
    ) -> Result<ConfigImportReport, EnvironmentError> {
        if request.files.is_empty() || request.files.len() > 16 {
            return Err(EnvironmentError::Invalid(
                "an import takes from 1 to 16 files".into(),
            ));
        }
        for file in &request.files {
            if file.name.is_empty()
                || file.name.len() > 128
                || file.name.contains(['/', '\\', '\0'])
                || file.content.len() > 1 << 20
            {
                return Err(EnvironmentError::Invalid(
                    "a file is named by its base name and holds at most 1 MiB".into(),
                ));
            }
        }
        let (merged, overridden) = merge_dotenv(&request.files)?;
        for name in request.public.iter().chain(&request.secret) {
            if !merged.contains_key(name) {
                return Err(EnvironmentError::Invalid(format!(
                    "{name} is named as public or secret but the files do not define it"
                )));
            }
        }
        if let Some(name) = request
            .public
            .iter()
            .find(|name| request.secret.contains(name))
        {
            return Err(EnvironmentError::Invalid(format!(
                "{name} is named as both public and secret"
            )));
        }
        let record = self.owned_environment(environment, operator).await?;
        self.require_live(&record).await?;

        let mut plan = vec![];
        let mut skipped = vec![];
        for (name, (value, source)) in &merged {
            if let Some(reason) = reserved(name) {
                skipped.push(SkippedVariable {
                    name: name.clone(),
                    reason: reason.into(),
                });
                continue;
            }
            let sensitive = if request.secret.contains(name) {
                true
            } else if request.public.contains(name) {
                false
            } else {
                !is_public(name)
            };
            let same = record.value.config.get(name) == Some(value)
                && record
                    .value
                    .configuration
                    .as_ref()
                    .and_then(|state| state.variables.get(name))
                    .is_some_and(|v| v.sensitive == sensitive && &v.source == source);
            plan.push((
                name.clone(),
                value.clone(),
                sensitive,
                source.clone(),
                !same,
            ));
        }
        let mut probe = BTreeMap::new();
        for (name, value, ..) in &plan {
            probe.insert(name.clone(), value.clone());
        }
        validate_env("environment", &probe)?;
        let files = request
            .files
            .iter()
            .map(|file| file.name.clone())
            .collect::<Vec<_>>();
        let imported = plan
            .iter()
            .map(|(name, _, sensitive, source, changed)| ImportedVariable {
                name: name.clone(),
                sensitive: *sensitive,
                source: source.clone(),
                changed: *changed,
            })
            .collect::<Vec<_>>();
        let unchanged = plan.iter().all(|(.., changed)| !changed);
        let generation = if unchanged {
            record
                .value
                .configuration
                .as_ref()
                .map_or(0, |state| state.generation)
        } else {
            let description = format!(
                "configuration imported from {} ({} variables)",
                files.join(", "),
                plan.iter().filter(|(.., changed)| *changed).count()
            );
            let view = self
                .change_environment(environment, operator, description, None, move |value| {
                    let state = value.configuration.get_or_insert_with(Default::default);
                    for (name, secret_value, sensitive, source, _) in &plan {
                        value.config.insert(name.clone(), secret_value.clone());
                        state.variables.insert(
                            name.clone(),
                            ConfigurationVariable {
                                sensitive: *sensitive,
                                source: source.clone(),
                            },
                        );
                    }
                    Ok(())
                })
                .await?;
            view.configuration.generation
        };
        Ok(ConfigImportReport {
            environment: environment.to_owned(),
            generation,
            unchanged,
            imported,
            skipped,
            overridden,
            files,
        })
    }

    /// Set and remove variables, atomically, with their treatment.
    pub async fn change_configuration(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        change: ConfigChange,
    ) -> Result<ConfigurationView, EnvironmentError> {
        validate_env("environment", &change.set)?;
        for name in change.public.iter().chain(&change.secret) {
            if !change.set.contains_key(name) {
                return Err(EnvironmentError::Invalid(format!(
                    "{name} is named as public or secret but is not being set"
                )));
            }
        }
        if let Some(name) = change
            .public
            .iter()
            .find(|name| change.secret.contains(name))
        {
            return Err(EnvironmentError::Invalid(format!(
                "{name} is named as both public and secret"
            )));
        }
        if change.set.is_empty() && change.unset.is_empty() {
            return self.configuration(environment, operator).await;
        }
        let source = change.source.clone().unwrap_or_else(|| "api".into());
        let description = format!(
            "configuration changed ({} set, {} removed)",
            change.set.len(),
            change.unset.len()
        );
        let view = self
            .change_environment(environment, operator, description, None, move |value| {
                let state = value.configuration.get_or_insert_with(Default::default);
                for name in &change.unset {
                    value.config.remove(name);
                    state.variables.remove(name);
                }
                for (name, secret_value) in &change.set {
                    let existing = state.variables.get(name).map(|v| v.sensitive);
                    let sensitive = if change.secret.contains(name) {
                        true
                    } else if change.public.contains(name) {
                        false
                    } else {
                        existing.unwrap_or_else(|| !is_public(name))
                    };
                    value.config.insert(name.clone(), secret_value.clone());
                    state.variables.insert(
                        name.clone(),
                        ConfigurationVariable {
                            sensitive,
                            source: source.clone(),
                        },
                    );
                }
                Ok(())
            })
            .await?;
        Ok(view.configuration)
    }

    /// What `.env`-style files in the environment's workspace ask for and
    /// provide: variable names only. The extraction runs in the computer and
    /// prints names; values never leave it.
    pub async fn discover_configuration(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
    ) -> Result<ConfigurationDiscovery, EnvironmentError> {
        let record = self.owned_environment(environment, operator).await?;
        let (_, client, session_id) = self.running(&record).await?;
        let (evidence, output) = self
            .run_in_computer_command(
                &client,
                &session_id,
                script(DISCOVER, []),
                Duration::from_secs(60),
            )
            .await;
        if evidence.outcome != "succeeded" {
            return Err(EnvironmentError::RuntimeUnavailable(format!(
                "discovering configuration in {environment} failed (job {})",
                evidence.job_id
            )));
        }
        let mut files: Vec<DiscoveredFile> = vec![];
        for line in output.lines() {
            if let Some(path) = line.strip_prefix("FILE ") {
                let kind = if path.ends_with(".env.example") {
                    "requirements"
                } else {
                    "values"
                };
                files.push(DiscoveredFile {
                    path: path.to_owned(),
                    kind: kind.into(),
                    variables: vec![],
                });
            } else if let (Some(name), Some(file)) = (line.strip_prefix("VAR "), files.last_mut())
                && !file.variables.contains(&name.to_owned())
            {
                file.variables.push(name.to_owned());
            }
        }
        let mut by_name = BTreeMap::<String, (bool, Vec<String>)>::new();
        for file in &files {
            for name in &file.variables {
                let entry = by_name.entry(name.clone()).or_default();
                entry.0 |= file.kind == "values";
                entry.1.push(file.path.clone());
            }
        }
        let variables = by_name
            .into_iter()
            .map(|(name, (supplied, files))| DiscoveredVariable {
                status: if record.value.config.contains_key(&name) {
                    "configured"
                } else if supplied {
                    "available"
                } else {
                    "missing"
                }
                .into(),
                sensitive: is_sensitive_by_default(&name),
                name,
                files,
            })
            .collect();
        Ok(ConfigurationDiscovery {
            environment: environment.to_owned(),
            files,
            variables,
        })
    }
}

fn is_sensitive_by_default(name: &str) -> bool {
    !is_public(name)
}

/// The names assigned in the standard `.env` files of the workspace root and of
/// each checked-out repository. It prints names and nothing else.
const DISCOVER: &str = r#"
set -eu
scan() {
  for f in .env .env.local .env.development .env.production .env.example; do
    p="$1/$f"
    [ -f "$p" ] || continue
    printf 'FILE %s\n' "${p#./}"
    sed -n -E 's/^[[:space:]]*(export[[:space:]]+)?([A-Za-z_][A-Za-z0-9_]*)[[:space:]]*=.*/VAR \2/p' "$p"
  done
}
scan .
for d in repos/*/; do
  [ -d "$d" ] && scan "${d%/}"
done
exit 0
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use compute_state::DesiredState;

    fn record(config: &[(&str, &str)]) -> EnvironmentRecord {
        EnvironmentRecord {
            name: "legacy".into(),
            desired_state: DesiredState::Running,
            config: config
                .iter()
                .map(|(name, value)| ((*name).into(), (*value).into()))
                .collect(),
            policy: None,
            provider: None,
            created_at: chrono::Utc::now(),
            owner: Some("alice".into()),
            computer: None,
            contents: None,
            configuration: None,
            recipe: None,
        }
    }

    #[test]
    fn a_record_that_predates_the_metadata_has_every_variable_sensitive() {
        let legacy = record(&[("APP_ENV", "prod"), ("TOKEN", "x")]);
        assert!(
            visible_config(&legacy).is_empty(),
            "nothing is known public, so nothing is shown"
        );
        let view = configuration_view("legacy", &legacy);
        assert!(
            view.variables
                .iter()
                .all(|v| v.sensitive && v.value.is_none() && v.source == "declared")
        );
        assert_eq!(view.generation, 0);

        // A change settles it: what existed stays sensitive, what is new is
        // classified by its name, and only a real change moves the generation.
        let mut after = legacy.clone();
        after.config.insert("APP_MODE".into(), "test".into());
        settle_configuration(Some(&legacy), &mut after, 5);
        let state = after.configuration.clone().unwrap();
        assert_eq!(state.generation, 5);
        assert!(
            state.variables["APP_ENV"].sensitive,
            "existing, unrecorded: sensitive"
        );
        assert!(
            !state.variables["APP_MODE"].sensitive,
            "new, public by name"
        );
        assert_eq!(
            visible_config(&after).keys().collect::<Vec<_>>(),
            vec!["APP_MODE"]
        );

        let mut same = after.clone();
        settle_configuration(Some(&after), &mut same, 9);
        assert_eq!(same.configuration.unwrap().generation, 5, "nothing changed");
    }

    #[test]
    fn generation_moves_with_a_value_a_variable_or_a_treatment() {
        let mut first = record(&[("A", "1")]);
        settle_configuration(None, &mut first, 1);
        assert_eq!(first.configuration.as_ref().unwrap().generation, 1);
        let mut value = first.clone();
        value.config.insert("A".into(), "2".into());
        settle_configuration(Some(&first), &mut value, 2);
        assert_eq!(
            value.configuration.as_ref().unwrap().generation,
            2,
            "a value"
        );
        let mut treatment = value.clone();
        treatment
            .configuration
            .as_mut()
            .unwrap()
            .variables
            .get_mut("A")
            .unwrap()
            .sensitive = false;
        settle_configuration(Some(&value), &mut treatment, 3);
        assert_eq!(
            treatment.configuration.as_ref().unwrap().generation,
            3,
            "a treatment"
        );
        let mut removed = treatment.clone();
        removed.config.clear();
        settle_configuration(Some(&treatment), &mut removed, 4);
        let state = removed.configuration.unwrap();
        assert_eq!(
            (state.generation, state.variables.len()),
            (4, 0),
            "a variable"
        );
    }

    #[test]
    fn replacing_what_a_caller_could_see_keeps_what_it_could_not() {
        let mut before = record(&[("PUBLIC_MODE", "a"), ("SECRET", "s")]);
        settle_configuration(None, &mut before, 1);
        // The caller saw only PUBLIC_MODE and sends it back with a new one.
        let update = BTreeMap::from([
            ("PUBLIC_MODE".to_string(), "b".to_string()),
            ("NEW".into(), "n".into()),
        ]);
        let merged = replace_visible_config(&before, &update);
        assert_eq!(merged["SECRET"], "s");
        assert_eq!(merged["PUBLIC_MODE"], "b");
        // Omitting a variable it could see removes it.
        let merged = replace_visible_config(&before, &BTreeMap::new());
        assert!(!merged.contains_key("PUBLIC_MODE") && merged.contains_key("SECRET"));
    }
}
