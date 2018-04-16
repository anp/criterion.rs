use stats::bivariate::Data;
use stats::bivariate::regression::Slope;
use report::{BenchmarkId, MeasurementData, Report, ReportContext};

use handlebars::Handlebars;
use fs;
use format;
use estimate::Statistic;
use Estimate;
use plot;
use criterion_plot::Size;
use stats::univariate::Sample;
use std::process::Child;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const THUMBNAIL_SIZE: Size = Size(450, 300);

fn wait_on_gnuplot(children: Vec<Child>) {
    let start = ::std::time::Instant::now();
    let child_count = children.len();
    for child in children {
        match child.wait_with_output() {
            Ok(ref out) if out.status.success() => {}
            Ok(out) => error!("Error in Gnuplot: {}", String::from_utf8_lossy(&out.stderr)),
            Err(e) => error!("Got IO error while waiting for Gnuplot to complete: {}", e),
        }
    }
    let elapsed = &start.elapsed();
    info!(
        "Waiting for {} gnuplot processes took {}",
        child_count,
        ::format::time(::DurationExt::to_nanos(elapsed) as f64)
    );
}

#[derive(Serialize)]
struct Context {
    title: String,
    confidence: String,

    thumbnail_width: usize,
    thumbnail_height: usize,

    slope: ConfidenceInterval,
    r2: ConfidenceInterval,
    mean: ConfidenceInterval,
    std_dev: ConfidenceInterval,
    median: ConfidenceInterval,
    mad: ConfidenceInterval,
    throughput: Option<ConfidenceInterval>,

    additional_plots: Vec<Plot>,

    comparison: Option<Comparison>,
}

#[derive(Serialize)]
struct IndividualBenchmark {
    name: String,
    path: String,
}
impl IndividualBenchmark {
    fn new(path_prefix: &str, id: &BenchmarkId) -> IndividualBenchmark {
        IndividualBenchmark {
            name: id.id().to_owned(),
            path: format!("{}/{}", path_prefix, id.id()),
        }
    }
}

#[derive(Serialize)]
struct SummaryContext {
    group_id: String,

    thumbnail_width: usize,
    thumbnail_height: usize,

    violin_plot: Option<PathBuf>,
    line_chart: Option<PathBuf>,

    benchmarks: Vec<IndividualBenchmark>,
}

#[derive(Serialize)]
struct ConfidenceInterval {
    lower: String,
    upper: String,
    point: String,
}

#[derive(Serialize)]
struct Plot {
    name: String,
    url: String,
}
impl Plot {
    fn new(name: &str, url: &str) -> Plot {
        Plot {
            name: name.to_owned(),
            url: url.to_owned(),
        }
    }
}

#[derive(Serialize)]
struct Comparison {
    p_value: String,
    inequality: String,
    significance_level: String,
    explanation: String,

    change: ConfidenceInterval,
    additional_plots: Vec<Plot>,
}

pub struct Html {
    handlebars: Handlebars,
}
impl Html {
    pub fn new() -> Html {
        let mut handlebars = Handlebars::new();
        handlebars
            .register_template_string("report", include_str!("benchmark_report.html.handlebars"))
            .expect("Unable to parse benchmark report template.");
        handlebars
            .register_template_string(
                "summary_report",
                include_str!("summary_report.html.handlebars"),
            )
            .expect("Unable to parse summary report template.");
        Html { handlebars }
    }
}
impl Report for Html {
    fn benchmark_start(&self, _: &BenchmarkId, _: &ReportContext) {}
    fn warmup(&self, _: &BenchmarkId, _: &ReportContext, _: f64) {}
    fn analysis(&self, _: &BenchmarkId, _: &ReportContext) {}
    fn measurement_start(&self, _: &BenchmarkId, _: &ReportContext, _: u64, _: f64, _: u64) {}
    fn measurement_complete(
        &self,
        id: &BenchmarkId,
        report_context: &ReportContext,
        measurements: &MeasurementData,
    ) {
        if !report_context.plotting.is_enabled() {
            return;
        }

        try_else_return!(fs::mkdirp(&report_context
            .output_directory
            .join(id.to_string())
            .join("report")));

        let slope_estimate = &measurements.absolute_estimates[&Statistic::Slope];

        fn time_interval(est: &Estimate) -> ConfidenceInterval {
            ConfidenceInterval {
                lower: format::time(est.confidence_interval.lower_bound),
                point: format::time(est.point_estimate),
                upper: format::time(est.confidence_interval.upper_bound),
            }
        }

        let data = Data::new(
            measurements.iter_counts.as_slice(),
            measurements.sample_times.as_slice(),
        );

        elapsed!{
            "Generating plots",
            self.generate_plots(id, report_context, measurements)
        }

        let throughput = measurements
            .throughput
            .as_ref()
            .map(|thr| ConfidenceInterval {
                lower: format::throughput(thr, slope_estimate.confidence_interval.upper_bound),
                upper: format::throughput(thr, slope_estimate.confidence_interval.lower_bound),
                point: format::throughput(thr, slope_estimate.point_estimate),
            });

        let context = Context {
            title: id.id().to_owned(),
            confidence: format!("{:.2}", slope_estimate.confidence_interval.confidence_level),

            thumbnail_width: THUMBNAIL_SIZE.0,
            thumbnail_height: THUMBNAIL_SIZE.1,

            slope: time_interval(slope_estimate),
            mean: time_interval(&measurements.absolute_estimates[&Statistic::Mean]),
            median: time_interval(&measurements.absolute_estimates[&Statistic::Median]),
            mad: time_interval(&measurements.absolute_estimates[&Statistic::MedianAbsDev]),
            std_dev: time_interval(&measurements.absolute_estimates[&Statistic::StdDev]),
            throughput: throughput,

            r2: ConfidenceInterval {
                lower: format!(
                    "{:0.7}",
                    Slope(slope_estimate.confidence_interval.lower_bound).r_squared(data)
                ),
                upper: format!(
                    "{:0.7}",
                    Slope(slope_estimate.confidence_interval.upper_bound).r_squared(data)
                ),
                point: format!(
                    "{:0.7}",
                    Slope(slope_estimate.point_estimate).r_squared(data)
                ),
            },

            additional_plots: vec![
                Plot::new("Slope", "slope.svg"),
                Plot::new("Mean", "mean.svg"),
                Plot::new("Std. Dev.", "SD.svg"),
                Plot::new("Median", "median.svg"),
                Plot::new("MAD", "MAD.svg"),
            ],

            comparison: self.comparison(measurements),
        };

        let text = self.handlebars
            .render("report", &context)
            .expect("Failed to render benchmark report template");
        try_else_return!(fs::save_string(
            &text,
            &report_context
                .output_directory
                .join(id.to_string())
                .join("report")
                .join("index.html")
        ));
    }

    fn summarize(&self, context: &ReportContext, all_ids: &[BenchmarkId]) {
        if !context.plotting.is_enabled() {
            return;
        }

        let mut all_plots = vec![];
        let group_id = &all_ids[0].group_id;

        let mut function_ids = BTreeSet::new();
        for id in all_ids.iter() {
            if let Some(ref function_id) = id.function_id {
                function_ids.insert(function_id);
            }
        }

        let data: Vec<(BenchmarkId, Vec<f64>)> =
            self.load_summary_data(&context.output_directory, all_ids);

        for function_id in function_ids {
            let samples_with_function: Vec<_> = data.iter()
                .by_ref()
                .filter(|&&(ref id, _)| id.function_id.as_ref() == Some(function_id))
                .collect();
            if samples_with_function.len() > 1 {
                let subgroup_id = format!("{}/{}", group_id, function_id);
                all_plots.extend(self.generate_summary(
                    &subgroup_id,
                    &*samples_with_function,
                    context,
                    false,
                ));
            }
        }

        all_plots.extend(self.generate_summary(
            group_id,
            &*(data.iter().by_ref().collect::<Vec<_>>()),
            context,
            true,
        ));
        wait_on_gnuplot(all_plots)
    }
}
impl Html {
    fn comparison(&self, measurements: &MeasurementData) -> Option<Comparison> {
        if let Some(ref comp) = measurements.comparison {
            let different_mean = comp.p_value < comp.significance_threshold;
            let mean_est = comp.relative_estimates[&Statistic::Mean];
            let explanation_str: String;

            if !different_mean {
                explanation_str = "No change in performance detected.".to_owned();
            } else {
                let comparison = compare_to_threshold(&mean_est, comp.noise_threshold);
                match comparison {
                    ComparisonResult::Improved => {
                        explanation_str = "Performance has improved.".to_owned();
                    }
                    ComparisonResult::Regressed => {
                        explanation_str = "Performance has regressed.".to_owned();
                    }
                    ComparisonResult::NonSignificant => {
                        explanation_str = "Change within noise threshold.".to_owned();
                    }
                }
            }

            let comp = Comparison {
                p_value: format!("{:.2}", comp.p_value),
                inequality: (if different_mean { "<" } else { ">" }).to_owned(),
                significance_level: format!("{:.2}", comp.significance_threshold),
                explanation: explanation_str,

                change: ConfidenceInterval {
                    point: format::change(mean_est.point_estimate, true),
                    lower: format::change(mean_est.confidence_interval.lower_bound, true),
                    upper: format::change(mean_est.confidence_interval.upper_bound, true),
                },

                additional_plots: vec![
                    Plot::new("Change in mean", "change/mean.svg"),
                    Plot::new("Change in median", "change/median.svg"),
                    Plot::new("T-Test", "change/t-test.svg"),
                ],
            };
            Some(comp)
        } else {
            None
        }
    }

    fn generate_plots(
        &self,
        id: &BenchmarkId,
        context: &ReportContext,
        measurements: &MeasurementData,
    ) {
        let data = Data::new(
            measurements.iter_counts.as_slice(),
            measurements.sample_times.as_slice(),
        );
        let slope_estimate = &measurements.absolute_estimates[&Statistic::Slope];
        let point = Slope::fit(data);
        let slope_dist = &measurements.distributions[&Statistic::Slope];
        let (lb, ub) =
            slope_dist.confidence_interval(slope_estimate.confidence_interval.confidence_level);
        let (lb_, ub_) = (Slope(lb), Slope(ub));

        let report_dir = context.output_directory.join(id.to_string()).join("report");
        let mut gnuplots = vec![];

        gnuplots.push(plot::pdf(
            data,
            measurements.avg_times,
            id,
            report_dir.join("pdf.svg"),
            None,
        ));
        gnuplots.extend(plot::abs_distributions(
            &measurements.distributions,
            &measurements.absolute_estimates,
            id,
            &context.output_directory,
        ));
        gnuplots.push(plot::regression(
            data,
            &point,
            (lb_, ub_),
            id,
            report_dir.join("regression.svg"),
            None,
            false,
        ));
        gnuplots.push(plot::pdf_small(
            &*measurements.avg_times,
            report_dir.join("pdf_small.svg"),
            Some(THUMBNAIL_SIZE),
        ));
        gnuplots.push(plot::regression(
            data,
            &point,
            (lb_, ub_),
            id,
            report_dir.join("regression_small.svg"),
            Some(THUMBNAIL_SIZE),
            true,
        ));

        if let Some(ref comp) = measurements.comparison {
            try_else_return!(fs::mkdirp(&report_dir.join("change")));

            let base_data = Data::new(&comp.base_iter_counts, &comp.base_sample_times);

            let both_dir = report_dir.join("both");
            try_else_return!(fs::mkdirp(&both_dir));
            gnuplots.push(plot::both::regression(
                base_data,
                &comp.base_estimates,
                data,
                &measurements.absolute_estimates,
                id,
                both_dir.join("regression.svg"),
                None,
                false,
            ));
            gnuplots.push(plot::both::pdfs(
                Sample::new(&comp.base_avg_times),
                &*measurements.avg_times,
                id,
                both_dir.join("pdf.svg"),
                None,
                false,
            ));
            gnuplots.push(plot::t_test(
                comp.t_value,
                &comp.t_distribution,
                id,
                &context.output_directory,
            ));
            gnuplots.extend(plot::rel_distributions(
                &comp.relative_distributions,
                &comp.relative_estimates,
                id,
                &context.output_directory,
                comp.noise_threshold,
            ));
            gnuplots.push(plot::both::regression(
                base_data,
                &comp.base_estimates,
                data,
                &measurements.absolute_estimates,
                id,
                report_dir.join("relative_regression_small.svg"),
                Some(THUMBNAIL_SIZE),
                true,
            ));
            gnuplots.push(plot::both::pdfs(
                Sample::new(&comp.base_avg_times),
                &*measurements.avg_times,
                id,
                report_dir.join("relative_pdf_small.svg"),
                Some(THUMBNAIL_SIZE),
                true,
            ));
        }

        wait_on_gnuplot(gnuplots);
    }

    fn load_summary_data<P: AsRef<Path>>(
        &self,
        output_dir: P,
        all_ids: &[BenchmarkId],
    ) -> Vec<(BenchmarkId, Vec<f64>)> {
        let output_dir = output_dir.as_ref();
        all_ids
            .iter()
            .filter_map(|id| {
                let entry = output_dir.join(id.id()).join("new");

                let (iters, times): (Vec<f64>, Vec<f64>) =
                    try_else_return!(fs::load(&entry.join("sample.json")), || None);
                let avg_times = iters
                    .into_iter()
                    .zip(times.into_iter())
                    .map(|(iters, time)| time / iters)
                    .collect::<Vec<_>>();

                Some((id.clone(), avg_times))
            })
            .collect::<Vec<_>>()
    }

    fn generate_summary(
        &self,
        group_id: &str,
        data: &[&(BenchmarkId, Vec<f64>)],
        report_context: &ReportContext,
        full_summary: bool,
    ) -> Vec<Child> {
        let mut gnuplots = vec![];

        let report_dir = report_context
            .output_directory
            .join(group_id)
            .join("report");

        try_else_return!(fs::mkdirp(&report_dir), || gnuplots);

        let violin_path = report_dir.join("violin.svg");

        gnuplots.push(plot::summary::violin(
            group_id,
            data,
            &violin_path,
            report_context.plot_config.summary_scale,
        ));

        let value_types: Vec<_> = data.iter().map(|&&(ref id, _)| id.value_type()).collect();
        let function_types: BTreeSet<_> =
            data.iter().map(|&&(ref id, _)| &id.function_id).collect();

        let mut line_path = None;

        if value_types.iter().all(|x| x == &value_types[0]) && function_types.len() > 1 {
            if let Some(value_type) = value_types[0] {
                let path = report_dir.join("lines.svg");

                gnuplots.push(plot::summary::line_comparison(
                    group_id,
                    data,
                    &path,
                    value_type,
                    report_context.plot_config.summary_scale,
                ));

                line_path = Some(path);
            }
        }

        let path_prefix = if full_summary {
            "../../.."
        } else {
            "../../../.."
        };
        let benchmarks = data.iter()
            .map(|&&(ref id, _)| IndividualBenchmark::new(path_prefix, id))
            .collect();

        let context = SummaryContext {
            group_id: group_id.to_owned(),

            thumbnail_width: THUMBNAIL_SIZE.0,
            thumbnail_height: THUMBNAIL_SIZE.1,

            violin_plot: Some(violin_path),
            line_chart: line_path,

            benchmarks: benchmarks,
        };

        let text = self.handlebars
            .render("summary_report", &context)
            .expect("Failed to render summary report template");
        try_else_return!(
            fs::save_string(&text, &report_dir.join("index.html")),
            || gnuplots
        );

        gnuplots
    }
}

enum ComparisonResult {
    Improved,
    Regressed,
    NonSignificant,
}

fn compare_to_threshold(estimate: &Estimate, noise: f64) -> ComparisonResult {
    let ci = estimate.confidence_interval;
    let lb = ci.lower_bound;
    let ub = ci.upper_bound;

    if lb < -noise && ub < -noise {
        ComparisonResult::Improved
    } else if lb > noise && ub > noise {
        ComparisonResult::Regressed
    } else {
        ComparisonResult::NonSignificant
    }
}
