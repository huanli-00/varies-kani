// Copyright Kani Contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use anyhow::{Error, Result, bail};
use kani_metadata::{HarnessKind, HarnessMetadata};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Write;
use std::path::Path;

use crate::args::OutputFormat;
use crate::call_cbmc::{ExitStatus, FailedProperties, VerificationResult, VerificationStatus};
use crate::cbmc_output_parser::CheckStatus;
use crate::coverage::cov_results::CoverageRegion;
use crate::project::{
    self, HarnessCoverageInput, HarnessCoverageStats, Project, SynthCoverageConfig,
    SynthHarnessScheduler, SynthSchedulerPlan, SynthTreeSnapshot,
};
use crate::session::{BUG_REPORT_URL, KaniSession};

use std::env::current_dir;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::{Level, debug, info, warn};

/// A HarnessRunner is responsible for checking all proof harnesses. The data in this structure represents
/// "background information" that the controlling driver (e.g. cargo-kani or kani) computed.
///
/// This struct is basically just a nicer way of passing many arguments to [`Self::check_all_harnesses`]
pub(crate) struct HarnessRunner<'sess, 'pr> {
    /// The underlying kani session
    pub sess: &'sess KaniSession,
    /// The project under verification.
    pub project: &'pr Project,
}

/// The result of checking a single harness. This both hangs on to the harness metadata
/// (as a means to identify which harness), and provides that harness's verification result.
pub(crate) struct HarnessResult<'pr> {
    pub harness: &'pr HarnessMetadata,
    pub result: VerificationResult,
}

/// Captures execution details for reporting.
pub(crate) struct HarnessExecutionReport<'pr> {
    pub executed: Vec<HarnessResult<'pr>>,
    pub waited: Vec<&'pr HarnessMetadata>,
    pub skipped: Vec<&'pr HarnessMetadata>,
    pub global_timeout: Option<Duration>,
    pub coverage_scope: project::SynthCoverageScope,
    pub skip_reasons: HashMap<*const HarnessMetadata, String>,
    pub target_by_harness: HashMap<*const HarnessMetadata, Option<usize>>,
    pub coverage_summary: Vec<project::CoverageSummary>,
    pub coverage_details: HashMap<Option<usize>, project::CoverageHistory>,
    pub synth_tree: Option<SynthTreeSnapshot>,
    pub harness_ids: HashMap<*const HarnessMetadata, String>,
    pub coverage_stats_by_harness: HashMap<*const HarnessMetadata, HarnessCoverageStats>,
}

fn build_coverage_input(result: &VerificationResult) -> Option<HarnessCoverageInput> {
    let coverage = result.coverage_results.as_ref()?;
    let mut covered_regions: HashSet<CoverageRegion> = HashSet::new();
    let mut uncovered_regions: HashSet<CoverageRegion> = HashSet::new();
    for checks in coverage.data.values() {
        for check in checks {
            match check.status() {
                CheckStatus::Covered => {
                    covered_regions.insert(check.region.clone());
                }
                CheckStatus::Uncovered => {
                    uncovered_regions.insert(check.region.clone());
                }
                _ => {}
            }
        }
    }
    for region in &covered_regions {
        uncovered_regions.remove(region);
    }
    Some(HarnessCoverageInput { covered_regions, uncovered_regions })
}

fn build_target_map<'pr>(
    scheduler: &SynthHarnessScheduler<'pr>,
    harnesses: &[&'pr HarnessMetadata],
) -> HashMap<*const HarnessMetadata, Option<usize>> {
    let mut map = HashMap::new();
    for harness in harnesses {
        let ptr = *harness as *const _;
        if let Some(target) = scheduler.target_for_harness(harness) {
            map.insert(ptr, target);
        }
    }
    map
}

fn is_harness_oom_error(error: &Error) -> bool {
    let message = format!("{error:#}").to_ascii_lowercase();
    message.contains("exit status: 137")
        || message.contains("status 137")
        || message.contains("sigkill")
        || message.contains("signal: 9")
}

fn failed_harness_result(exit_status: ExitStatus, runtime: Duration) -> VerificationResult {
    VerificationResult {
        status: VerificationStatus::Failure,
        failed_properties: FailedProperties::Other,
        results: Err(exit_status),
        runtime,
        generated_concrete_test: false,
        coverage_results: None,
    }
}

impl<'pr> HarnessRunner<'_, 'pr> {
    /// Given a [`HarnessRunner`] (to abstract over how these harnesses were generated), this runs
    /// the proof-checking process for each harness in `harnesses`.
    pub(crate) fn check_all_harnesses(
        &self,
        harnesses: &'pr [&HarnessMetadata],
    ) -> Result<HarnessExecutionReport<'pr>> {
        let include_untracked = self.sess.args.target_idx.is_none();
        let coverage_config = SynthCoverageConfig::new(
            self.sess.args.synth_successor_k_min,
            self.sess.args.synth_successor_k_divisor,
            self.sess.args.synth_delta_threshold,
            self.sess.args.synth_uncovered_cnt_threshold,
            self.sess.args.synth_uncovered_ratio_threshold,
            self.sess.args.synth_score_delta_weight,
            self.sess.args.synth_score_uncovered_weight,
            self.sess.args.synth_coverage_scope,
            self.sess.args.synth_pruning,
        );
        if let Some(plan) =
            project::build_synth_harness_scheduler(harnesses, include_untracked, coverage_config)
        {
            info!("Using synthesized harness scheduler for checking harnesses");
            let SynthSchedulerPlan { scheduler, harnesses } = plan;
            return self.check_with_synth_scheduler(harnesses, scheduler);
        }
        Err(Error::msg(
            "Failed to build synthesized harness scheduler; cannot proceed with verification.",
        ))
    }
}

impl<'pr> HarnessRunner<'_, 'pr> {
    fn check_with_synth_scheduler(
        &self,
        harnesses: Vec<&'pr HarnessMetadata>,
        mut scheduler: SynthHarnessScheduler<'pr>,
    ) -> Result<HarnessExecutionReport<'pr>> {
        let mut results = Vec::new();
        let configured_timeout: Option<Duration> =
            self.sess.args.global_timeout.map(Duration::from);
        let target_timeout: Option<Duration> = self.sess.args.target_timeout.map(Duration::from);
        let mut target_elapsed: HashMap<Option<usize>, Duration> = HashMap::new();
        let timeout_pruning_enabled = scheduler.timeout_pruning_enabled();
        let start_time = Instant::now();
        let deadline = configured_timeout.map(|limit| start_time + limit);
        let mut timed_out = false;

        loop {
            if let Some(deadline) = deadline {
                if Instant::now() >= deadline {
                    timed_out = true;
                    break;
                }
            }

            let Some(harness) = scheduler.next() else {
                break;
            };

            let harness_target = scheduler.target_for_harness(harness).unwrap_or(None);
            if timeout_pruning_enabled {
                if let (Some(limit), Some(target)) = (target_timeout, harness_target) {
                    let elapsed = target_elapsed.get(&Some(target)).copied().unwrap_or_default();
                    if elapsed >= limit {
                        scheduler.mark_target_timed_out(Some(target));
                        scheduler.abandon_inflight(harness);
                        continue;
                    }
                }
            }

            let target_started_at = Instant::now();
            let goto_artifact = match self.project.ensure_goto_artifact(self.sess, harness) {
                Ok(artifact) => artifact,
                Err(error) => {
                    if harness_target.is_some() {
                        *target_elapsed.entry(harness_target).or_default() +=
                            target_started_at.elapsed();
                    }
                    if is_harness_oom_error(&error) {
                        warn!(
                            harness = %harness.pretty_name,
                            ?error,
                            "Treating goto preparation OOM as harness failure"
                        );
                        let result = failed_harness_result(
                            ExitStatus::OutOfMemory,
                            target_started_at.elapsed(),
                        );
                        scheduler.record_result(
                            harness,
                            result.runtime.as_secs_f64(),
                            result.status,
                            None,
                        );
                        results.push(HarnessResult { harness, result });
                        if self.sess.args.fail_fast {
                            let skip_reasons = scheduler.skip_reasons_for(&harnesses);
                            let target_by_harness = build_target_map(&scheduler, &harnesses);
                            let coverage_summary = scheduler.coverage_summary();
                            let coverage_details = scheduler.coverage_details();
                            let coverage_scope = scheduler.coverage_scope();
                            let synth_tree = Some(scheduler.snapshot());
                            let harness_ids = scheduler.harness_ids();
                            let coverage_stats_by_harness = scheduler.coverage_stats_by_harness();
                            return Ok(build_execution_report_from_vec(
                                harnesses,
                                results,
                                vec![],
                                None,
                                coverage_scope,
                                skip_reasons,
                                target_by_harness,
                                coverage_summary,
                                coverage_details,
                                synth_tree,
                                harness_ids,
                                coverage_stats_by_harness,
                            ));
                        }
                        continue;
                    }
                    warn!(
                        harness = %harness.pretty_name,
                        ?error,
                        "Skipping harness due to goto preparation failure"
                    );
                    continue;
                }
            };
            let goto_file = goto_artifact.as_ref();

            if let Err(error) =
                self.sess.instrument_model(goto_file, goto_file, self.project, harness)
            {
                if harness_target.is_some() {
                    *target_elapsed.entry(harness_target).or_default() +=
                        target_started_at.elapsed();
                }
                if is_harness_oom_error(&error) {
                    warn!(
                        harness = %harness.pretty_name,
                        ?error,
                        "Treating instrumentation OOM as harness failure"
                    );
                    let result =
                        failed_harness_result(ExitStatus::OutOfMemory, target_started_at.elapsed());
                    scheduler.record_result(
                        harness,
                        result.runtime.as_secs_f64(),
                        result.status,
                        None,
                    );
                    results.push(HarnessResult { harness, result });
                    if self.sess.args.fail_fast {
                        let skip_reasons = scheduler.skip_reasons_for(&harnesses);
                        let target_by_harness = build_target_map(&scheduler, &harnesses);
                        let coverage_summary = scheduler.coverage_summary();
                        let coverage_details = scheduler.coverage_details();
                        let coverage_scope = scheduler.coverage_scope();
                        let synth_tree = Some(scheduler.snapshot());
                        let harness_ids = scheduler.harness_ids();
                        let coverage_stats_by_harness = scheduler.coverage_stats_by_harness();
                        return Ok(build_execution_report_from_vec(
                            harnesses,
                            results,
                            vec![],
                            None,
                            coverage_scope,
                            skip_reasons,
                            target_by_harness,
                            coverage_summary,
                            coverage_details,
                            synth_tree,
                            harness_ids,
                            coverage_stats_by_harness,
                        ));
                    }
                    continue;
                }
                return Err(error);
            }

            if self.sess.args.synthesize_loop_contracts {
                if let Err(error) =
                    self.sess.synthesize_loop_contracts(goto_file, goto_file, harness)
                {
                    if harness_target.is_some() {
                        *target_elapsed.entry(harness_target).or_default() +=
                            target_started_at.elapsed();
                    }
                    if is_harness_oom_error(&error) {
                        warn!(
                            harness = %harness.pretty_name,
                            ?error,
                            "Treating loop-contract synthesis OOM as harness failure"
                        );
                        let result = failed_harness_result(
                            ExitStatus::OutOfMemory,
                            target_started_at.elapsed(),
                        );
                        scheduler.record_result(
                            harness,
                            result.runtime.as_secs_f64(),
                            result.status,
                            None,
                        );
                        results.push(HarnessResult { harness, result });
                        if self.sess.args.fail_fast {
                            let skip_reasons = scheduler.skip_reasons_for(&harnesses);
                            let target_by_harness = build_target_map(&scheduler, &harnesses);
                            let coverage_summary = scheduler.coverage_summary();
                            let coverage_details = scheduler.coverage_details();
                            let coverage_scope = scheduler.coverage_scope();
                            let synth_tree = Some(scheduler.snapshot());
                            let harness_ids = scheduler.harness_ids();
                            let coverage_stats_by_harness = scheduler.coverage_stats_by_harness();
                            return Ok(build_execution_report_from_vec(
                                harnesses,
                                results,
                                vec![],
                                None,
                                coverage_scope,
                                skip_reasons,
                                target_by_harness,
                                coverage_summary,
                                coverage_details,
                                synth_tree,
                                harness_ids,
                                coverage_stats_by_harness,
                            ));
                        }
                        continue;
                    }
                    return Err(error);
                }
            }

            let start = Instant::now();
            let result = self.sess.check_harness(goto_file, harness)?;
            if harness_target.is_some() {
                *target_elapsed.entry(harness_target).or_default() += target_started_at.elapsed();
            }
            let duration_secs = start.elapsed().as_secs_f64();
            debug!(
                "Checked synthesized harness {} in {:.2} seconds",
                harness.pretty_name, duration_secs
            );
            let coverage_input = build_coverage_input(&result);
            scheduler.record_result(harness, duration_secs, result.status, coverage_input);

            if self.sess.args.fail_fast && result.status == VerificationStatus::Failure {
                results.push(HarnessResult { harness, result });
                let skip_reasons = scheduler.skip_reasons_for(&harnesses);
                let target_by_harness = build_target_map(&scheduler, &harnesses);
                let coverage_summary = scheduler.coverage_summary();
                let coverage_details = scheduler.coverage_details();
                let coverage_scope = scheduler.coverage_scope();
                let synth_tree = Some(scheduler.snapshot());
                let harness_ids = scheduler.harness_ids();
                let coverage_stats_by_harness = scheduler.coverage_stats_by_harness();
                return Ok(build_execution_report_from_vec(
                    harnesses,
                    results,
                    vec![],
                    None,
                    coverage_scope,
                    skip_reasons,
                    target_by_harness,
                    coverage_summary,
                    coverage_details,
                    synth_tree,
                    harness_ids,
                    coverage_stats_by_harness,
                ));
            }

            results.push(HarnessResult { harness, result });

            if let Some(deadline) = deadline {
                if Instant::now() >= deadline {
                    timed_out = true;
                    break;
                }
            }
        }

        let waited = if timed_out { scheduler.pending_harnesses() } else { Vec::new() };
        let timeout_hit = if timed_out { configured_timeout } else { None };

        let skip_reasons = scheduler.skip_reasons_for(&harnesses);
        let target_by_harness = build_target_map(&scheduler, &harnesses);
        let coverage_summary = scheduler.coverage_summary();
        let coverage_details = scheduler.coverage_details();
        let coverage_scope = scheduler.coverage_scope();
        let synth_tree = Some(scheduler.snapshot());
        let harness_ids = scheduler.harness_ids();
        let coverage_stats_by_harness = scheduler.coverage_stats_by_harness();
        Ok(build_execution_report_from_vec(
            harnesses,
            results,
            waited,
            timeout_hit,
            coverage_scope,
            skip_reasons,
            target_by_harness,
            coverage_summary,
            coverage_details,
            synth_tree,
            harness_ids,
            coverage_stats_by_harness,
        ))
    }
}

fn build_execution_report_from_vec<'pr>(
    harnesses: Vec<&'pr HarnessMetadata>,
    executed: Vec<HarnessResult<'pr>>,
    waited: Vec<&'pr HarnessMetadata>,
    global_timeout: Option<Duration>,
    coverage_scope: project::SynthCoverageScope,
    skip_reasons: HashMap<*const HarnessMetadata, String>,
    target_by_harness: HashMap<*const HarnessMetadata, Option<usize>>,
    coverage_summary: Vec<project::CoverageSummary>,
    coverage_details: HashMap<Option<usize>, project::CoverageHistory>,
    synth_tree: Option<SynthTreeSnapshot>,
    harness_ids: HashMap<*const HarnessMetadata, String>,
    coverage_stats_by_harness: HashMap<*const HarnessMetadata, HarnessCoverageStats>,
) -> HarnessExecutionReport<'pr> {
    let executed_set: HashSet<*const HarnessMetadata> =
        executed.iter().map(|result| result.harness as *const _).collect();
    let waited_set: HashSet<*const HarnessMetadata> =
        waited.iter().map(|h| *h as *const _).collect();
    let skipped = harnesses
        .into_iter()
        .filter(|h| {
            let ptr = *h as *const _;
            !executed_set.contains(&ptr) && !waited_set.contains(&ptr)
        })
        .collect();

    HarnessExecutionReport {
        executed,
        waited,
        skipped,
        global_timeout,
        coverage_scope,
        skip_reasons,
        target_by_harness,
        coverage_summary,
        coverage_details,
        synth_tree,
        harness_ids,
        coverage_stats_by_harness,
    }
}

impl KaniSession {
    fn process_output(
        &self,
        result: &VerificationResult,
        harness: &HarnessMetadata,
        thread_index: usize,
    ) {
        if self.should_print_output() {
            if self.args.output_into_files {
                self.write_output_to_file(result, harness, thread_index);
            }

            let output = result.render(&self.args.output_format, harness.attributes.should_panic);
            if rayon::current_num_threads() > 1 {
                println!("Thread {thread_index}: {output}");
            } else {
                println!("{output}");
            }
        }
    }

    fn should_print_output(&self) -> bool {
        !self.args.common_args.quiet && self.args.output_format != OutputFormat::Old
    }

    fn write_output_to_file(
        &self,
        result: &VerificationResult,
        harness: &HarnessMetadata,
        thread_index: usize,
    ) {
        let target_dir = self.result_output_dir().unwrap();
        let file_name = target_dir.join(harness.pretty_name.clone());
        let path = Path::new(&file_name);
        let prefix = path.parent().unwrap();

        std::fs::create_dir_all(prefix).unwrap();
        let mut file = File::create(&file_name).unwrap();
        let mut file_output =
            result.render(&OutputFormat::Regular, harness.attributes.should_panic);
        if rayon::current_num_threads() > 1 {
            file_output = format!("Thread {thread_index}:\n{file_output}");
        }

        if let Err(e) = writeln!(file, "{file_output}") {
            eprintln!(
                "Failed to write to file {}: {}",
                file_name.into_os_string().into_string().unwrap(),
                e
            );
        }
    }

    fn result_output_dir(&self) -> Result<PathBuf> {
        let target_dir = self.args.target_dir.clone().map_or_else(current_dir, Ok)?;
        Ok(target_dir.join("result_output_dir")) //Hardcode output to result_output_dir, may want to make it adjustable?
    }

    /// Run the verification process for a single harness
    pub(crate) fn check_harness(
        &self,
        binary: &Path,
        harness: &HarnessMetadata,
    ) -> Result<VerificationResult> {
        let thread_index = rayon::current_thread_index().unwrap_or_default();
        if !self.args.common_args.quiet {
            // If the harness is automatically generated, pretty_name refers to the function under verification.
            let mut msg = if harness.is_automatically_generated {
                if matches!(harness.attributes.kind, HarnessKind::Proof) {
                    format!(
                        "Autoharness: Checking function {} against all possible inputs...",
                        harness.pretty_name
                    )
                } else {
                    format!(
                        "Autoharness: Checking function {}'s contract against all possible inputs...",
                        harness.pretty_name
                    )
                }
            } else {
                format!("Checking harness {}...", harness.pretty_name)
            };

            if rayon::current_num_threads() > 1 {
                msg = format!("Thread {thread_index}: {msg}");
            }

            println!("{msg}");
        }

        let mut result = self.with_timer(|| self.run_cbmc(binary, harness), "run_cbmc")?;

        self.process_output(&result, harness, thread_index);
        self.gen_and_add_concrete_playback(harness, &mut result)?;
        Ok(result)
    }

    /// Concludes a session by printing a summary report and exiting the process with an
    /// error code (if applicable).
    ///
    /// Note: Takes `self` "by ownership". This function wants to be able to drop before
    /// exiting with an error code, if needed.
    pub(crate) fn print_final_summary(self, report: HarnessExecutionReport<'_>) -> Result<()> {
        if self.args.html_output {
            if let Err(error) = write_synth_html_reports(&report) {
                warn!(?error, "Failed to write synthesized HTML report");
            }
        }

        if self.args.common_args.quiet {
            return Ok(());
        }

        print_execution_order(&report);

        let results = &report.executed;
        let skipped = report.skipped.len();
        let waited = report.waited.len();

        let (automatic, manual): (Vec<_>, Vec<_>) =
            results.iter().partition(|r| r.harness.is_automatically_generated);

        let (successes, failures): (Vec<_>, Vec<_>) =
            manual.into_iter().partition(|r| r.result.status == VerificationStatus::Success);

        let succeeding = successes.len();
        let failing = failures.len();
        let total = succeeding + failing;

        if self.args.concrete_playback.is_some() {
            if failures.is_empty() {
                println!(
                    "INFO: The concrete playback feature never generated unit tests because there were no failing harnesses."
                )
            } else if failures.iter().all(|r| !r.result.generated_concrete_test) {
                eprintln!(
                    "The concrete playback feature did not generate unit tests, but there were failing harnesses. Please file a bug report at {BUG_REPORT_URL}"
                )
            }
        }

        println!("Manual Harness Summary:");

        for failure in failures.iter() {
            println!("Verification failed for - {}", failure.harness.pretty_name);
        }

        if total > 0 || skipped > 0 || waited > 0 {
            let skipped_msg =
                if skipped > 0 { format!(", {skipped} skipped") } else { String::new() };
            let waited_msg = if waited > 0 { format!(", {waited} waited") } else { String::new() };
            println!(
                "Complete - {succeeding} successfully verified harnesses, {failing} failures{skipped_msg}{waited_msg}, {} total considered.",
                total + skipped + waited
            );
            if total == 0 {
                println!("All considered harnesses were skipped; no harnesses were executed.");
            }
        } else {
            match self.args.harnesses.as_slice() {
                [] =>
                // TODO: This could use a better message, possibly with links to Kani documentation.
                // New users may encounter this and could use a pointer to how to write proof harnesses.
                {
                    println!(
                        "No proof harnesses (functions with #[kani::proof]) were found to verify."
                    )
                }
                [harness] => {
                    bail!("no harnesses matched the harness filter: `{harness}`")
                }
                harnesses => {
                    bail!("no harnesses matched the harness filters: `{}`", harnesses.join("`, `"))
                }
            };
        }

        if let Some(limit) = report.global_timeout {
            println!(
                "Global timeout of {} applied; {} harness(es) waited for verification.",
                format_duration(limit),
                waited
            );
        }

        if self.args.coverage {
            self.show_coverage_summary()?;
        }

        let autoharness_failing = if self.autoharness_compiler_flags.is_some() {
            self.print_autoharness_summary(automatic)?
        } else {
            0
        };

        if failing + autoharness_failing + waited > 0 {
            // Failure exit code without additional error message
            drop(self);
            std::process::exit(1);
        }

        Ok(())
    }

    /// Show a coverage summary.
    ///
    /// This is just a placeholder for now.
    fn show_coverage_summary(&self) -> Result<()> {
        Ok(())
    }
}

fn print_execution_order(report: &HarnessExecutionReport<'_>) {
    if report.executed.is_empty() && report.skipped.is_empty() && report.waited.is_empty() {
        println!("No harness executions were performed.");
        return;
    }

    if let Some(limit) = report.global_timeout {
        println!(
            "Global verification timeout ({}) reached; pending harnesses marked as waited.",
            format_duration(limit)
        );
    }

    println!("Harness Verification Order:");

    let mut target_order: Vec<Option<usize>> = Vec::new();
    let mut seen_targets = HashSet::new();
    let mut record_target = |target: Option<usize>| {
        if seen_targets.insert(target) {
            target_order.push(target);
        }
    };

    for entry in &report.executed {
        let target =
            report.target_by_harness.get(&(entry.harness as *const _)).copied().unwrap_or(None);
        record_target(target);
    }
    for harness in &report.waited {
        let target = report.target_by_harness.get(&(*harness as *const _)).copied().unwrap_or(None);
        record_target(target);
    }
    for harness in &report.skipped {
        let target = report.target_by_harness.get(&(*harness as *const _)).copied().unwrap_or(None);
        record_target(target);
    }

    for target in target_order {
        println!("{}", render_target_header(target));
        let mut index = 1;
        for entry in report.executed.iter() {
            let harness_target =
                report.target_by_harness.get(&(entry.harness as *const _)).copied().unwrap_or(None);
            if harness_target == target {
                let status = render_status(&entry.result);
                println!("{:>3}. {} - {}", index, entry.harness.pretty_name, status);
                index += 1;
            }
        }
        for harness in report.waited.iter() {
            let harness_target =
                report.target_by_harness.get(&(*harness as *const _)).copied().unwrap_or(None);
            if harness_target == target {
                println!("{:>3}. {} - waited", index, harness.pretty_name);
                index += 1;
            }
        }
        for harness in report.skipped.iter() {
            let harness_target =
                report.target_by_harness.get(&(*harness as *const _)).copied().unwrap_or(None);
            if harness_target == target {
                let reason = report.skip_reasons.get(&(*harness as *const _)).cloned();
                if let Some(reason) = reason {
                    println!("{:>3}. {} - skipped ({reason})", index, harness.pretty_name);
                } else {
                    println!("{:>3}. {} - skipped", index, harness.pretty_name);
                }
                index += 1;
            }
        }
    }

    if !report.coverage_summary.is_empty() {
        println!("Synthesized Coverage Summary:");
        for summary in &report.coverage_summary {
            println!(
                "  {} - covered_regions={}, uncovered_regions={}",
                render_coverage_scope_label(summary.target, report.coverage_scope),
                summary.covered_cnt,
                summary.uncovered_cnt
            );
        }
        if tracing::enabled!(Level::DEBUG) && !report.coverage_details.is_empty() {
            let mut targets: Vec<_> = report.coverage_details.keys().copied().collect();
            targets.sort_by(|a, b| a.cmp(b));
            for target in targets {
                if let Some(history) = report.coverage_details.get(&target) {
                    println!(
                        "  {} covered regions:",
                        render_coverage_scope_label(target, report.coverage_scope)
                    );
                    for region in history.covered_regions() {
                        println!("    {region}");
                    }
                    println!(
                        "  {} uncovered regions:",
                        render_coverage_scope_label(target, report.coverage_scope)
                    );
                    for region in history.uncovered_regions() {
                        println!("    {region}");
                    }
                }
            }
        }
    }
}

fn render_target_label(target: Option<usize>) -> String {
    match target {
        Some(idx) => format!("target {idx}"),
        None => "target unknown".to_string(),
    }
}

fn render_coverage_scope_label(
    target: Option<usize>,
    coverage_scope: project::SynthCoverageScope,
) -> String {
    match coverage_scope {
        project::SynthCoverageScope::Global => "global".to_string(),
        project::SynthCoverageScope::Target => render_target_label(target),
    }
}

fn render_target_header(target: Option<usize>) -> String {
    format!("-- {} --", render_target_label(target))
}

fn write_synth_html_reports(report: &HarnessExecutionReport<'_>) -> Result<()> {
    let Some(tree) = &report.synth_tree else {
        return Ok(());
    };
    let debug_enabled = tracing::enabled!(Level::DEBUG);

    let out_dir = current_dir()?.join("kani_results");
    std::fs::create_dir_all(&out_dir)?;

    let mut nodes_by_id = HashMap::new();
    let mut children_by_id: HashMap<String, Vec<String>> = HashMap::new();
    let mut roots_by_target: HashMap<Option<usize>, Vec<String>> = HashMap::new();

    for node in &tree.nodes {
        nodes_by_id.insert(node.id.clone(), node);
        children_by_id.insert(node.id.clone(), node.children.clone());
        if node.parent.is_none() {
            roots_by_target.entry(node.target).or_default().push(node.id.clone());
        }
    }

    let mut targets: Vec<Option<usize>> = roots_by_target.keys().copied().collect();
    targets.sort_by(|a, b| a.cmp(b));

    let mut status_by_id: HashMap<String, String> = HashMap::new();
    let mut order_by_id: HashMap<String, usize> = HashMap::new();
    let mut time_by_id: HashMap<String, f64> = HashMap::new();

    let mut order_counters: HashMap<Option<usize>, usize> = HashMap::new();
    for entry in report.executed.iter() {
        let ptr = entry.harness as *const _;
        if let Some(id) = report.harness_ids.get(&ptr) {
            let target = report.target_by_harness.get(&ptr).copied().unwrap_or(None);
            let counter = order_counters.entry(target).or_insert(0);
            *counter += 1;
            status_by_id.insert(id.clone(), render_status(&entry.result).to_string());
            order_by_id.insert(id.clone(), *counter);
            time_by_id.insert(id.clone(), entry.result.runtime.as_secs_f64());
        }
    }
    for harness in &report.waited {
        let ptr = *harness as *const _;
        if let Some(id) = report.harness_ids.get(&ptr) {
            status_by_id.insert(id.clone(), "waited".to_string());
        }
    }
    for harness in &report.skipped {
        let ptr = *harness as *const _;
        if let Some(id) = report.harness_ids.get(&ptr) {
            let reason =
                report.skip_reasons.get(&ptr).cloned().unwrap_or_else(|| "skipped".to_string());
            status_by_id.insert(id.clone(), format!("skipped ({reason})"));
        }
    }

    let mut coverage_stats_by_id: HashMap<String, HarnessCoverageStats> = HashMap::new();
    for (ptr, stats) in &report.coverage_stats_by_harness {
        if let Some(id) = report.harness_ids.get(ptr) {
            coverage_stats_by_id.insert(id.clone(), stats.clone());
        }
    }

    let mut index_html = String::new();
    index_html.push_str("<!doctype html><html><head><meta charset=\"utf-8\"><title>Varying Kani Results</title></head><body>");
    index_html.push_str("<h1>Synthesized Harness Verification</h1><ul>");

    for target in targets {
        let file_name = match target {
            Some(idx) => format!("synth_target_{idx}.html"),
            None => "synth_target_unknown.html".to_string(),
        };
        let label = render_target_label(target);
        index_html.push_str(&format!("<li><a href=\"{}\">{}</a></li>", file_name, label));
        write_target_html(
            &out_dir.join(&file_name),
            target,
            report.coverage_scope,
            &roots_by_target,
            &nodes_by_id,
            &children_by_id,
            &status_by_id,
            &order_by_id,
            &time_by_id,
            &coverage_stats_by_id,
            &report.coverage_details,
            debug_enabled,
        )?;
    }
    index_html.push_str("</ul></body></html>");
    std::fs::write(out_dir.join("index.html"), index_html)?;

    Ok(())
}

fn write_target_html(
    path: &Path,
    target: Option<usize>,
    coverage_scope: project::SynthCoverageScope,
    roots_by_target: &HashMap<Option<usize>, Vec<String>>,
    nodes_by_id: &HashMap<String, &project::SynthTreeNode>,
    children_by_id: &HashMap<String, Vec<String>>,
    status_by_id: &HashMap<String, String>,
    order_by_id: &HashMap<String, usize>,
    time_by_id: &HashMap<String, f64>,
    coverage_stats_by_id: &HashMap<String, HarnessCoverageStats>,
    coverage_details: &HashMap<Option<usize>, project::CoverageHistory>,
    debug_enabled: bool,
) -> Result<()> {
    let mut html = String::new();
    html.push_str("<!doctype html><html><head><meta charset=\"utf-8\">");
    html.push_str("<style>");
    html.push_str("body{font-family:Arial,Helvetica,sans-serif;margin:20px;} .node{margin:4px 0;} .status{font-weight:bold;} .success{color:#1b5e20;} .failure{color:#b71c1c;} .timeout{color:#f57f17;} .skipped{color:#546e7a;} ul{list-style-type:none;padding-left:18px;border-left:1px solid #ddd;} li{margin:6px 0;} details{margin-top:4px;} .meta{color:#616161;font-size:0.9em;} </style>");
    html.push_str("</head><body>");
    html.push_str(&format!("<h1>Target {}</h1>", render_target_label(target)));

    let roots = roots_by_target.get(&target).cloned().unwrap_or_default();
    html.push_str("<h2>Dependency Tree</h2>");
    if roots.is_empty() {
        html.push_str("<p>No roots found for this target.</p>");
    } else {
        html.push_str("<ul>");
        for root in roots {
            write_tree_node(
                &mut html,
                &root,
                nodes_by_id,
                children_by_id,
                status_by_id,
                order_by_id,
                time_by_id,
                coverage_stats_by_id,
                debug_enabled,
            );
        }
        html.push_str("</ul>");
    }

    let history = match coverage_scope {
        project::SynthCoverageScope::Global => coverage_details.get(&None),
        project::SynthCoverageScope::Target => coverage_details.get(&target),
    };
    if let Some(history) = history {
        html.push_str("<h2>Coverage Summary</h2>");
        html.push_str("<details><summary>Covered regions</summary><ul>");
        let mut covered_regions = history.covered_regions();
        covered_regions.sort_by(|a, b| a.file.cmp(&b.file).then(a.start.cmp(&b.start)));
        for region in covered_regions {
            let label = format!("{}:{}", region.file, region);
            html.push_str(&format!("<li>{}</li>", escape_html(&label)));
        }
        html.push_str("</ul></details>");
        html.push_str("<details><summary>Uncovered regions</summary><ul>");
        let mut uncovered_regions = history.uncovered_regions();
        uncovered_regions.sort_by(|a, b| a.file.cmp(&b.file).then(a.start.cmp(&b.start)));
        for region in uncovered_regions {
            let label = format!("{}:{}", region.file, region);
            html.push_str(&format!("<li>{}</li>", escape_html(&label)));
        }
        html.push_str("</ul></details>");
    }

    html.push_str("</body></html>");
    std::fs::write(path, html)?;
    Ok(())
}

fn write_tree_node(
    out: &mut String,
    node_id: &str,
    nodes_by_id: &HashMap<String, &project::SynthTreeNode>,
    children_by_id: &HashMap<String, Vec<String>>,
    status_by_id: &HashMap<String, String>,
    order_by_id: &HashMap<String, usize>,
    time_by_id: &HashMap<String, f64>,
    coverage_stats_by_id: &HashMap<String, HarnessCoverageStats>,
    debug_enabled: bool,
) {
    let Some(node) = nodes_by_id.get(node_id) else {
        return;
    };
    out.push_str("<li>");
    let status = status_by_id.get(node_id).cloned().unwrap_or_else(|| "pending".to_string());
    let status_class = if status.starts_with("success") {
        "success"
    } else if status.starts_with("failure") {
        "failure"
    } else if status.starts_with("timeout") {
        "timeout"
    } else if status.starts_with("skipped") {
        "skipped"
    } else {
        "meta"
    };
    let order = order_by_id.get(node_id).copied();
    let time = time_by_id.get(node_id).copied();
    let mut line = String::new();
    line.push_str(&format!(
        "<span class=\"status {}\">{}</span> ",
        status_class,
        escape_html(&status)
    ));
    line.push_str(&escape_html(&node.pretty_name));
    if let Some(order) = order {
        line.push_str(&format!(" <span class=\"meta\">order={}</span>", order));
    }
    if let Some(time) = time {
        line.push_str(&format!(" <span class=\"meta\">time={:.3}s</span>", time));
    }
    if let Some(stats) = coverage_stats_by_id.get(node_id) {
        line.push_str(&format!(
            " <span class=\"meta\">cov c={} u={} uf={} d={} r={:.3}</span>",
            stats.covered_cnt,
            stats.uncovered_cnt,
            stats.uncovered_filtered_cnt,
            stats.delta_covered_cnt,
            stats.uncovered_ratio()
        ));
    }

    let children = children_by_id.get(node_id).cloned().unwrap_or_default();
    if !children.is_empty() {
        let open_attr = if status.starts_with("success") { " open" } else { "" };
        out.push_str(&format!("<details{}><summary class=\"node\">{}</summary>", open_attr, line));
        if debug_enabled {
            if let Some(stats) = coverage_stats_by_id.get(node_id) {
                out.push_str("<details><summary>coverage details</summary>");
                out.push_str(&format!(
                    "<div class=\"meta\">covered={} uncovered={} uncovered_filtered={} delta={} ratio={:.3}</div>",
                    stats.covered_cnt,
                    stats.uncovered_cnt,
                    stats.uncovered_filtered_cnt,
                    stats.delta_covered_cnt,
                    stats.uncovered_ratio()
                ));
                out.push_str("</details>");
            }
        }
        out.push_str("<ul>");
        for child_id in children {
            write_tree_node(
                out,
                &child_id,
                nodes_by_id,
                children_by_id,
                status_by_id,
                order_by_id,
                time_by_id,
                coverage_stats_by_id,
                debug_enabled,
            );
        }
        out.push_str("</ul></details>");
    } else {
        out.push_str(&format!("<div class=\"node\">{}</div>", line));
        if debug_enabled {
            if let Some(stats) = coverage_stats_by_id.get(node_id) {
                out.push_str("<details><summary>coverage details</summary>");
                out.push_str(&format!(
                    "<div class=\"meta\">covered={} uncovered={} uncovered_filtered={} delta={} ratio={:.3}</div>",
                    stats.covered_cnt,
                    stats.uncovered_cnt,
                    stats.uncovered_filtered_cnt,
                    stats.delta_covered_cnt,
                    stats.uncovered_ratio()
                ));
                out.push_str("</details>");
            }
        }
    }
    out.push_str("</li>");
    out.push_str("</li>");
}

fn escape_html(input: &str) -> String {
    let mut escaped = String::new();
    for ch in input.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn render_status(result: &VerificationResult) -> &'static str {
    match result.status {
        VerificationStatus::Success => "success",
        VerificationStatus::Failure => match &result.results {
            Err(ExitStatus::Timeout) => "timeout",
            _ => "failure",
        },
    }
}

fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs == 0 {
        format!("{:.2}s", duration.as_secs_f64())
    } else if secs % 3600 == 0 {
        format!("{}h", secs / 3600)
    } else if secs % 60 == 0 {
        format!("{}m", secs / 60)
    } else if duration.subsec_millis() == 0 {
        format!("{secs}s")
    } else {
        format!("{:.2}s", duration.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::is_harness_oom_error;
    use anyhow::anyhow;

    #[test]
    fn classifies_signal_9_and_137_as_harness_oom() {
        assert!(is_harness_oom_error(&anyhow!(
            "goto-instrument exited with status signal: 9 (SIGKILL)"
        )));
        assert!(is_harness_oom_error(&anyhow!("cargo exited with status exit status: 137")));
        assert!(!is_harness_oom_error(&anyhow!(
            "goto-instrument exited with status exit status: 1"
        )));
    }
}
