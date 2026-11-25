use slimetrail::{
    Coverage, Error, JobEstimate, Projection, Result, config,
    index::{
        self,
        estimate::{duration, size},
    },
};
use std::io::{BufRead, Write};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(super) struct Choice {
    pub(super) coverage: Coverage,
    pub(super) save: bool,
}

fn total(estimate: &JobEstimate, coverage: Coverage) -> Projection {
    estimate
        .agents
        .iter()
        .map(|agent| agent.projection(coverage))
        .fold(
            Projection {
                index_bytes: 0,
                millis: 0,
            },
            |total, projection| Projection {
                index_bytes: total.index_bytes.saturating_add(projection.index_bytes),
                millis: total.millis.saturating_add(projection.millis),
            },
        )
}

fn option(coverage: Coverage, projection: &Projection, recommended: bool) -> String {
    let cost = format!(
        "~{}, ~{}",
        size(projection.index_bytes),
        duration(projection.millis)
    );
    let note = if recommended { "   (recommended)" } else { "" };
    format!("  {coverage:<8} {cost:<20}{note}")
        .trim_end()
        .to_owned()
}

fn declined(limit_ms: Option<u64>, output: &mut dyn Write) -> std::io::Result<()> {
    limit_ms.map_or(Ok(()), |limit| {
        writeln!(
            output,
            "No builds only what fits in {}; `slimetrail index` builds the rest.",
            duration(limit)
        )
    })
}

pub(super) fn ask(
    estimate: &JobEstimate,
    config: config::Config,
    limit_ms: Option<u64>,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<Option<Choice>> {
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
            writeln!(output, "{mismatch}.").map_err(io)?;
        }
        declined(limit_ms, output).map_err(io)?;
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
        declined(limit_ms, output).map_err(io)?;
        write!(output, "Build now? [c]ompact or [y]es / [f]ull / [N]o: ").map_err(io)?;
    }
    output.flush().map_err(io)?;
    let mut answer = String::new();
    input.read_line(&mut answer).map_err(io)?;
    let answer = answer.trim().to_ascii_lowercase();
    if fixed {
        return Ok(matches!(answer.as_str(), "y" | "yes").then_some(Choice {
            coverage: estimate.coverage,
            save: false,
        }));
    }
    let coverage = match answer.as_str() {
        "c" | "compact" | "y" | "yes" => Coverage::Compact,
        "f" | "full" => Coverage::Full,
        _ => return Ok(None),
    };
    Ok(Some(Choice {
        coverage,
        save: true,
    }))
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

    fn answer(text: &str, config: config::Config) -> (Option<Choice>, String) {
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
    ) -> (Option<Choice>, String) {
        let mut output = Vec::new();
        let chosen = ask(
            estimate,
            config,
            Some(120_000),
            &mut text.as_bytes(),
            &mut output,
        )
        .unwrap();
        (chosen, String::from_utf8(output).unwrap())
    }

    const fn chose(coverage: Coverage, save: bool) -> Choice {
        Choice { coverage, save }
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
        let (_, output) = answer_for(&estimate, "n\n", config::Config::default());
        assert!(output.starts_with(
            "slimetrail needs to build its index (4,300 sessions, 3.5 GB of logs).\n  compact  ~1.0 GB, ~2 min"
        ), "{output}");
    }

    #[test]
    fn unset_coverage_offers_both_presets_and_saves_the_choice() {
        let unset = config::Config::default();
        let (chosen, output) = answer("c\n", unset);
        assert_eq!(chosen, Some(chose(Coverage::Compact, true)));
        assert_eq!(
            output,
            "slimetrail needs to build its index (4,210 sessions, 3.1 GB of logs).\n  compact  ~900 MB, ~2 min        (recommended)\n  full     ~6.0 GB, ~9 min\nNo builds only what fits in 2 min; `slimetrail index` builds the rest.\nBuild now? [c]ompact or [y]es / [f]ull / [N]o: "
        );
        assert_eq!(answer("F\n", unset).0, Some(chose(Coverage::Full, true)));
        assert_eq!(answer("y\n", unset).0, Some(chose(Coverage::Compact, true)));
        for declined in ["n\n", "\n", ""] {
            assert_eq!(answer(declined, unset).0, None, "{declined:?}");
        }
    }

    #[test]
    fn explicit_coverage_asks_yes_or_no_without_rewriting_config() {
        let full = config::Config {
            coverage: Coverage::Full,
            explicit: true,
        };
        let (chosen, output) = answer("y\n", full);
        assert_eq!(chosen, Some(chose(Coverage::Full, false)));
        assert_eq!(
            output,
            "slimetrail needs to build its index (4,210 sessions, 3.1 GB of logs).\n  full     ~6.0 GB, ~9 min\nNo builds only what fits in 2 min; `slimetrail index` builds the rest.\nBuild now? [y/N]: "
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
        let (chosen, output) = answer_for(&locked, "y\n", full);
        assert_eq!(chosen, Some(chose(Coverage::Compact, false)));
        assert_eq!(
            output,
            "slimetrail needs to build its index (4,210 sessions, 3.1 GB of logs).\n  compact  ~900 MB, ~2 min\nThe config asks for full coverage but the index is compact; run `slimetrail index` to rebuild it.\nNo builds only what fits in 2 min; `slimetrail index` builds the rest.\nBuild now? [y/N]: "
        );
        let unset = config::Config::default();
        let (chosen, output) = answer_for(&locked, "f\n", unset);
        assert_eq!(chosen, None);
        assert!(
            output.ends_with("~2 min\nNo builds only what fits in 2 min; `slimetrail index` builds the rest.\nBuild now? [y/N]: "),
            "{output}"
        );
        assert_eq!(answer_for(&locked, "", unset).0, None);
    }
}
