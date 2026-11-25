use slimetrail::{
    Coverage, Error, JobEstimate, Projection, Result, config,
    index::{
        self,
        estimate::{duration, size},
    },
};
use std::{
    io::{BufRead, Write},
    path::Path,
};

pub(super) fn total(estimate: &JobEstimate, coverage: Coverage) -> Projection {
    let mut total = Projection {
        index_bytes: 0,
        millis: 0,
    };
    for agent in &estimate.agents {
        let projection = match coverage {
            Coverage::Compact => &agent.compact,
            Coverage::Full => &agent.full,
        };
        total.index_bytes = total.index_bytes.saturating_add(projection.index_bytes);
        total.millis = total.millis.saturating_add(projection.millis);
    }
    total
}

const fn name(coverage: Coverage) -> &'static str {
    match coverage {
        Coverage::Compact => "compact",
        Coverage::Full => "full",
    }
}

fn option(coverage: Coverage, projection: &Projection, recommended: bool) -> String {
    let name = name(coverage);
    let cost = format!(
        "~{}, ~{}",
        size(projection.index_bytes),
        duration(projection.millis)
    );
    let note = if recommended { "   (recommended)" } else { "" };
    format!("  {name:<8} {cost:<20}{note}")
        .trim_end()
        .to_owned()
}

pub(super) fn ask(
    estimate: &JobEstimate,
    config: config::Config,
    path: &Path,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<Option<Coverage>> {
    let io = |source| Error::Io {
        path: "<terminal>".into(),
        source,
    };
    let sessions = estimate.agents.iter().map(|a| a.sessions).sum();
    let bytes = estimate.agents.iter().map(|a| a.bytes).sum();
    writeln!(
        output,
        "slimetrail needs to build its index ({}, {} of logs).",
        index::estimate::sessions(sessions),
        size(bytes)
    )
    .map_err(io)?;
    let fixed = estimate.coverage_locked || config.explicit;
    if fixed {
        let coverage = estimate.coverage;
        writeln!(
            output,
            "{}",
            option(coverage, &total(estimate, coverage), false)
        )
        .map_err(io)?;
        if let Some(mismatch) = index::coverage::mismatch(coverage, config.coverage) {
            writeln!(output, "{mismatch}").map_err(io)?;
        }
        write!(output, "Build now? [y/N]: ").map_err(io)?;
    } else {
        for (coverage, recommended) in [(Coverage::Compact, true), (Coverage::Full, false)] {
            writeln!(
                output,
                "{}",
                option(coverage, &total(estimate, coverage), recommended)
            )
            .map_err(io)?;
        }
        write!(output, "Build now? [c]ompact / [f]ull / [N]o: ").map_err(io)?;
    }
    output.flush().map_err(io)?;
    let mut answer = String::new();
    input.read_line(&mut answer).map_err(io)?;
    let answer = answer.trim().to_ascii_lowercase();
    if fixed {
        return Ok(matches!(answer.as_str(), "y" | "yes").then_some(estimate.coverage));
    }
    let chosen = match answer.as_str() {
        "c" | "compact" | "y" | "yes" => Coverage::Compact,
        "f" | "full" => Coverage::Full,
        _ => return Ok(None),
    };
    config::write_coverage(path, chosen)?;
    Ok(Some(chosen))
}

#[cfg(test)]
mod tests {
    use super::*;
    use slimetrail::{Agent, AgentEstimate};

    fn estimate_at(coverage: Coverage, coverage_locked: bool) -> JobEstimate {
        JobEstimate {
            coverage,
            coverage_locked,
            agents: vec![AgentEstimate {
                agent: Agent::Claude,
                sessions: 4_210,
                bytes: 3_100_000_000,
                compact: Projection {
                    index_bytes: 900_000_000,
                    millis: 110_000,
                },
                full: Projection {
                    index_bytes: 6_000_000_000,
                    millis: 540_000,
                },
            }],
        }
    }

    fn answer(text: &str, config: config::Config) -> (Option<Coverage>, String, Option<String>) {
        let coverage = if config.explicit {
            config.coverage
        } else {
            Coverage::Compact
        };
        answer_for(&estimate_at(coverage, false), text, config)
    }

    fn answer_for(
        estimate: &JobEstimate,
        text: &str,
        config: config::Config,
    ) -> (Option<Coverage>, String, Option<String>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut output = Vec::new();
        let chosen = ask(estimate, config, &path, &mut text.as_bytes(), &mut output).unwrap();
        (
            chosen,
            String::from_utf8(output).unwrap(),
            std::fs::read_to_string(path).ok(),
        )
    }

    #[test]
    fn several_agents_are_summed_into_one_line() {
        let mut estimate = estimate_at(Coverage::Compact, true);
        estimate.agents.push(AgentEstimate {
            agent: Agent::Codex,
            sessions: 90,
            bytes: 400_000_000,
            compact: Projection {
                index_bytes: 100_000_000,
                millis: 10_000,
            },
            full: Projection {
                index_bytes: 700_000_000,
                millis: 60_000,
            },
        });
        let (_, output, _) = answer_for(&estimate, "n\n", config::Config::default());
        assert!(output.starts_with(
            "slimetrail needs to build its index (4,300 sessions, 3.5 GB of logs).\n  compact  ~1.0 GB, ~2 min"
        ), "{output}");
    }

    #[test]
    fn unset_coverage_offers_both_presets_and_saves_the_choice() {
        let unset = config::Config::default();
        let (chosen, output, saved) = answer("c\n", unset);
        assert_eq!(chosen, Some(Coverage::Compact));
        assert_eq!(saved.as_deref(), Some("coverage = \"compact\"\n"));
        assert_eq!(
            output,
            "slimetrail needs to build its index (4,210 sessions, 3.1 GB of logs).\n  compact  ~900 MB, ~2 min        (recommended)\n  full     ~6.0 GB, ~9 min\nBuild now? [c]ompact / [f]ull / [N]o: "
        );
        let (chosen, _, saved) = answer("F\n", unset);
        assert_eq!(chosen, Some(Coverage::Full));
        assert_eq!(saved.as_deref(), Some("coverage = \"full\"\n"));
        assert_eq!(answer("y\n", unset).0, Some(Coverage::Compact));
        for declined in ["n\n", "\n", ""] {
            let (chosen, _, saved) = answer(declined, unset);
            assert_eq!((chosen, saved), (None, None), "{declined:?}");
        }
    }

    #[test]
    fn explicit_coverage_asks_yes_or_no_without_rewriting_config() {
        let full = config::Config {
            coverage: Coverage::Full,
            explicit: true,
        };
        let (chosen, output, saved) = answer("y\n", full);
        assert_eq!((chosen, saved), (Some(Coverage::Full), None));
        assert_eq!(
            output,
            "slimetrail needs to build its index (4,210 sessions, 3.1 GB of logs).\n  full     ~6.0 GB, ~9 min\nBuild now? [y/N]: "
        );
        for declined in ["n\n", "\n", "", "c\n"] {
            assert_eq!(answer(declined, full).0, None, "{declined:?}");
        }
    }

    #[test]
    fn locked_index_offers_only_its_coverage_and_names_the_switch() {
        let full = config::Config {
            coverage: Coverage::Full,
            explicit: true,
        };
        let locked = estimate_at(Coverage::Compact, true);
        let (chosen, output, saved) = answer_for(&locked, "y\n", full);
        assert_eq!((chosen, saved), (Some(Coverage::Compact), None));
        assert_eq!(
            output,
            "slimetrail needs to build its index (4,210 sessions, 3.1 GB of logs).\n  compact  ~900 MB, ~2 min\nThe config asks for full coverage but the index is compact; run `slimetrail index` to rebuild it.\nBuild now? [y/N]: "
        );
        let unset = config::Config::default();
        let (chosen, output, saved) = answer_for(&locked, "f\n", unset);
        assert_eq!((chosen, saved), (None, None));
        assert!(output.ends_with("~2 min\nBuild now? [y/N]: "), "{output}");
        assert_eq!(answer_for(&locked, "", unset).0, None);
    }
}
