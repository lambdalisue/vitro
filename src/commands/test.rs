//! `vitro test` — run a project's command on each of its images.
//!
//! Each image gets what `run --rm --sync` gives one: a fresh VM, the project
//! copied in, the command, and the VM destroyed afterwards. The images are
//! taken one at a time; two guests side by side would halve what each gets
//! from the host and interleave their output.

use anyhow::{bail, Result};

use crate::commands::run;
use crate::{ssh, Config, Paths, Project};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The command ran, with this exit code.
    Exited(i32),
    /// No VM came up, so the command never ran.
    DidNotStart(String),
    /// The VM came up but the command could not be run in it, most often
    /// because the project could not be sent.
    DidNotRun(String),
}

pub struct Options {
    /// Overrides the project's own list when not empty.
    pub images: Vec<String>,
    /// Appended to the project's command.
    pub extra: Vec<String>,
}

pub fn test(paths: &Paths, config: &Config, project: &Project, options: Options) -> Result<i32> {
    let images = if options.images.is_empty() {
        project.images.clone()
    } else {
        options.images
    };
    if images.is_empty() {
        bail!(
            "name an image, or list the ones this project runs on as `images` in {}",
            project.dir.join(crate::PROJECT_FILE).display()
        );
    }
    // Every name checked before the first VM starts, so a typo in the last
    // one does not surface after the first has run for twenty minutes.
    for image in &images {
        config.image(image)?;
    }

    let mut command = project.command.clone();
    command.extend(options.extra);

    let mut results = Vec::new();
    for image in &images {
        eprintln!("vitro: testing on {image}");
        let outcome = match run::run(
            paths,
            config,
            run::Options {
                image: image.clone(),
                name: None,
                sizing: run::Sizing {
                    cpus: project.cpus,
                    memory: project.memory,
                },
            },
        ) {
            // A failure here belongs to this image alone; the others still
            // run, and the summary still names every one.
            Ok(record) => match run::then(
                paths,
                config,
                &record.name,
                run::Then {
                    command: command.clone(),
                    sync: project.sync.clone(),
                    prelude: ssh::Prelude {
                        env: project.env.clone(),
                        ..ssh::Prelude::default()
                    },
                    remove: true,
                },
            ) {
                Ok(code) => Outcome::Exited(code),
                Err(e) => Outcome::DidNotRun(format!("{e:#}")),
            },
            Err(e) => Outcome::DidNotStart(format!("{e:#}")),
        };
        results.push((image.clone(), outcome));
        if crate::signals::interrupted() {
            break;
        }
    }

    eprint!("{}", summary(&results));
    Ok(exit_code(&results))
}

/// One line per image, so a run over several guests ends with the verdict
/// rather than with the last guest's output.
pub fn summary(results: &[(String, Outcome)]) -> String {
    let width = results
        .iter()
        .map(|(image, _)| image.len())
        .max()
        .unwrap_or(0);
    results
        .iter()
        .map(|(image, outcome)| {
            let verdict = match outcome {
                Outcome::Exited(0) => "passed".to_string(),
                Outcome::Exited(code) => format!("failed (exit code {code})"),
                Outcome::DidNotStart(reason) => format!("did not start: {reason}"),
                Outcome::DidNotRun(reason) => format!("did not run: {reason}"),
            };
            format!("{image:<width$}  {verdict}\n")
        })
        .collect()
}

/// A lone image's own exit code, as `run` would give; across several, the
/// first failure's, so the caller still sees a guest's code where there is one.
pub fn exit_code(results: &[(String, Outcome)]) -> i32 {
    results
        .iter()
        .find_map(|(_, outcome)| match outcome {
            Outcome::Exited(0) => None,
            Outcome::Exited(code) => Some(*code),
            Outcome::DidNotStart(_) | Outcome::DidNotRun(_) => Some(1),
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn results(outcomes: &[(&str, Outcome)]) -> Vec<(String, Outcome)> {
        outcomes
            .iter()
            .map(|(image, outcome)| (image.to_string(), outcome.clone()))
            .collect()
    }

    #[test]
    fn every_image_passing_is_success() {
        let results = results(&[
            ("windows", Outcome::Exited(0)),
            ("macos", Outcome::Exited(0)),
        ]);

        assert_eq!(exit_code(&results), 0);
    }

    #[test]
    fn the_first_failure_decides_the_exit_code() {
        let results = results(&[
            ("windows", Outcome::Exited(0)),
            ("macos", Outcome::Exited(101)),
            ("linux", Outcome::Exited(2)),
        ]);

        assert_eq!(exit_code(&results), 101);
    }

    #[test]
    fn a_command_that_could_not_be_run_is_a_failure_with_its_reason() {
        let results = results(&[("windows", Outcome::DidNotRun("could not copy".into()))]);

        assert_eq!(exit_code(&results), 1);
        assert_eq!(summary(&results), "windows  did not run: could not copy\n");
    }

    #[test]
    fn a_guest_that_never_started_is_a_failure() {
        let results = results(&[("windows", Outcome::DidNotStart("no SSH answer".into()))]);

        assert_eq!(exit_code(&results), 1);
    }

    #[test]
    fn the_summary_has_one_aligned_verdict_per_image() {
        let text = summary(&results(&[
            ("windows", Outcome::Exited(0)),
            ("mac", Outcome::Exited(101)),
            ("linux", Outcome::DidNotStart("no SSH answer".into())),
        ]));

        assert_eq!(
            text,
            "windows  passed\n\
             mac      failed (exit code 101)\n\
             linux    did not start: no SSH answer\n"
        );
    }
}
