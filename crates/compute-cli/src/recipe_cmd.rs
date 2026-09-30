//! `compute recipe`: lifecycle policy, declared as data.
//!
//! A recipe is a `compute.recipe@1` document (see `docs/recipes.md`). These
//! commands list, read, write, validate, and explain recipes; they never
//! run anything. A recipe is used by `compute environment create --recipe`,
//! which sends the ordinary environment request the recipe resolves to.

use std::path::PathBuf;

use clap::{Args, Subcommand};
use compute_core::{ComputeError, RecipeSpec};
use compute_environment::{
    RecipeDefinition, RecipeResolution, RecipeResolveRequest, RecipeVerdict, RecipeView,
};

use crate::environment_cmd::{DaemonLocation, error, print_json};

/// Exit status of `validate` for a valid recipe no target can host: the
/// status a failed placement exits with everywhere else.
const UNSATISFIED_EXIT: i32 = crate::pool::PLACEMENT_FAILED_EXIT;
/// Exit status of `validate` for a recipe that is itself invalid.
const INVALID_EXIT: i32 = 3;

#[derive(Args, Debug)]
pub struct RecipeCommand {
    #[command(subcommand)]
    pub command: RecipeCommands,
    #[command(flatten)]
    pub daemon: DaemonLocation,
}

#[derive(Subcommand, Debug)]
pub enum RecipeCommands {
    /// List recipes at their current version.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show a recipe: its current version, or `--version N`.
    Get {
        name: String,
        #[arg(long)]
        version: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Check a recipe without acquiring anything: is it valid, and does any
    /// target satisfy it now? Exits 0 when it can be used, 2 when it is
    /// valid but no target satisfies it, 3 when the recipe is invalid.
    Validate(ResolveArgs),
    /// Explain what a recipe will cause Compute to do, before it does it.
    Resolve(ResolveArgs),
    /// Create a recipe from a `compute.recipe@1` JSON file.
    Create {
        name: String,
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Write a recipe's next version from a JSON file. Refused with
    /// `conflict` if the recipe changed since the version you name (default:
    /// the version current now).
    Edit {
        name: String,
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        expected_version: Option<u64>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug)]
pub struct ResolveArgs {
    /// A stored recipe.
    #[arg(required_unless_present = "file", conflicts_with = "file")]
    pub name: Option<String>,
    /// A draft spec (JSON), not stored: preview before creating.
    #[arg(long)]
    pub file: Option<PathBuf>,
    #[arg(long, requires = "name")]
    pub version: Option<u64>,
    /// Constrain placement to this target (a recipe never names one).
    #[arg(long)]
    pub target: Option<String>,
    #[arg(long)]
    pub json: bool,
}

fn read_spec(path: &PathBuf) -> compute_core::Result<RecipeSpec> {
    serde_json::from_slice(&std::fs::read(path)?)
        .map_err(|error| ComputeError::InvalidWorkload(format!("{}: {error}", path.display())))
}

/// Ask the control plane what a recipe resolves to. Nothing is acquired.
pub(crate) async fn resolution(
    client: &compute_environment::client::DaemonClient,
    name: &str,
    version: Option<u64>,
    target: Option<&str>,
) -> compute_core::Result<RecipeResolution> {
    let mut path = format!("/recipes/{name}/resolve");
    let mut query = vec![];
    if let Some(version) = version {
        query.push(format!("version={version}"));
    }
    if let Some(target) = target {
        query.push(format!("target={target}"));
    }
    if !query.is_empty() {
        path = format!("{path}?{}", query.join("&"));
    }
    client.get(&path).await.map_err(error)
}

/// The refusal for a recipe that cannot be used, in the terms that tell the
/// caller what to fix: the recipe, or the pool.
pub(crate) fn unusable(name: &str, resolution: &RecipeResolution) -> Option<ComputeError> {
    match resolution.verdict {
        RecipeVerdict::Satisfiable => None,
        RecipeVerdict::Invalid => Some(ComputeError::InvalidWorkload(format!(
            "recipe {name} is invalid: {}",
            resolution.problems.join("; ")
        ))),
        RecipeVerdict::Unsatisfied => Some(ComputeError::Runtime(format!(
            "no target satisfies recipe {name}: {}",
            unsatisfied_reasons(resolution)
        ))),
    }
}

fn unsatisfied_reasons(resolution: &RecipeResolution) -> String {
    let Some(placement) = &resolution.placement else {
        return "placement was not evaluated".into();
    };
    if placement.providers.is_empty() {
        return "the pool has no targets".into();
    }
    placement
        .providers
        .iter()
        .map(|provider| {
            format!(
                "{}: {}",
                provider.provider_id,
                provider
                    .reasons
                    .iter()
                    .map(|reason| reason.code.as_str().to_owned())
                    .chain(provider.error.iter().map(|error| error.message.clone()))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn print_view(view: &RecipeView, json: bool) {
    if json {
        print_json(view);
        return;
    }
    println!(
        "{} version {} ({}), digest {}",
        view.name, view.version, view.status, view.digest
    );
    println!("author {} at {}", view.author, view.created_at.to_rfc3339());
    print_json(&view.spec);
}

fn print_resolution(resolution: &RecipeResolution) {
    match &resolution.recipe {
        Some(recipe) => println!(
            "Recipe: {} version {} ({})",
            recipe.name, recipe.version, recipe.digest
        ),
        None => println!("Recipe: draft (not stored)"),
    }
    match resolution.verdict {
        RecipeVerdict::Invalid => {
            println!("Verdict: invalid: the recipe cannot resolve");
            for problem in &resolution.problems {
                println!("  - {problem}");
            }
            return;
        }
        RecipeVerdict::Unsatisfied => println!(
            "Verdict: unsatisfied: the recipe is valid, and no current target satisfies it\n  {}",
            unsatisfied_reasons(resolution)
        ),
        RecipeVerdict::Satisfiable => {
            let target = resolution
                .placement
                .as_ref()
                .and_then(|placement| placement.selected.as_ref())
                .map(|selected| selected.provider_id.as_str())
                .unwrap_or("?");
            println!("Verdict: satisfiable: placement would select {target}");
        }
    }
    if let Some(resolved) = &resolution.resolved {
        println!("Resolves to (the request `environment create` takes):");
        print_json(resolved);
    }
    if !resolution.implied_capabilities.is_empty() {
        println!(
            "Also required by the lifecycle: {}",
            resolution.implied_capabilities.join(", ")
        );
    }
    println!("Lifecycle:");
    for line in &resolution.lifecycle {
        println!("  - {line}");
    }
    println!("Carried out by existing operations:");
    for step in &resolution.primitives {
        println!("  - {}: {}", step.operation, step.effect);
    }
}

async fn resolve_args(
    client: &compute_environment::client::DaemonClient,
    args: &ResolveArgs,
) -> compute_core::Result<(String, RecipeResolution)> {
    match (&args.name, &args.file) {
        (Some(name), _) => Ok((
            name.clone(),
            resolution(client, name, args.version, args.target.as_deref()).await?,
        )),
        (None, Some(file)) => {
            let request = RecipeResolveRequest {
                spec: Some(read_spec(file)?),
                target: args.target.clone(),
            };
            Ok((
                file.display().to_string(),
                client
                    .post("/recipes/resolve", Some(&request))
                    .await
                    .map_err(error)?,
            ))
        }
        (None, None) => unreachable!("clap requires a recipe or a file"),
    }
}

pub async fn recipe(command: RecipeCommand) -> compute_core::Result<()> {
    let client = command.daemon.client()?;
    match command.command {
        RecipeCommands::List { json } => {
            let recipes: Vec<RecipeView> = client.get("/recipes").await.map_err(error)?;
            if json {
                print_json(&recipes);
            } else {
                println!("RECIPE\tVERSION\tLIFECYCLE\tDESCRIPTION");
                for recipe in recipes {
                    println!(
                        "{}\t{}\t{}\t{}",
                        recipe.name,
                        recipe.version,
                        recipe.spec.lifecycle.as_str(),
                        recipe.spec.description.as_deref().unwrap_or("")
                    );
                }
            }
        }
        RecipeCommands::Get {
            name,
            version,
            json,
        } => {
            let path = match version {
                Some(version) => format!("/recipes/{name}?version={version}"),
                None => format!("/recipes/{name}"),
            };
            print_view(&client.get(&path).await.map_err(error)?, json);
        }
        RecipeCommands::Resolve(args) => {
            let (_, resolution) = resolve_args(&client, &args).await?;
            if args.json {
                print_json(&resolution);
            } else {
                print_resolution(&resolution);
            }
        }
        RecipeCommands::Validate(args) => {
            let (_, resolution) = resolve_args(&client, &args).await?;
            if args.json {
                print_json(&resolution);
            } else {
                print_resolution(&resolution);
            }
            match resolution.verdict {
                RecipeVerdict::Satisfiable => {}
                RecipeVerdict::Unsatisfied => std::process::exit(UNSATISFIED_EXIT),
                RecipeVerdict::Invalid => std::process::exit(INVALID_EXIT),
            }
        }
        RecipeCommands::Create { name, file, json } => {
            let definition = RecipeDefinition {
                name,
                spec: read_spec(&file)?,
                expected_version: None,
            };
            let view: RecipeView = client
                .post("/recipes", Some(&definition))
                .await
                .map_err(error)?;
            print_view(&view, json);
        }
        RecipeCommands::Edit {
            name,
            file,
            expected_version,
            json,
        } => {
            let expected = match expected_version {
                Some(version) => version,
                None => {
                    client
                        .get::<RecipeView>(&format!("/recipes/{name}"))
                        .await
                        .map_err(error)?
                        .version
                }
            };
            let definition = RecipeDefinition {
                name,
                spec: read_spec(&file)?,
                expected_version: Some(expected),
            };
            let view: RecipeView = client
                .post("/recipes", Some(&definition))
                .await
                .map_err(error)?;
            print_view(&view, json);
        }
    }
    Ok(())
}
