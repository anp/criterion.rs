use std::path::Path;
use std::collections::BTreeMap;

use stats::{Distribution, Tails};
use stats::bivariate::Data;
use stats::bivariate::regression::Slope;
use stats::univariate::Sample;
use stats::univariate::outliers::tukey::{self, LabeledSample};

use estimate::{Distributions, Estimates, Statistic};
use metrics::EventName;
use routine::Routine;
use benchmark::BenchmarkConfig;
use {ConfidenceInterval, Criterion, Estimate, Throughput};
use {format, fs};
use report::{BenchmarkId, ReportContext};

macro_rules! elapsed {
    ($msg:expr, $block:expr) => ({
        let start = ::std::time::Instant::now();
        let out = $block;
        let elapsed = &start.elapsed();

        info!("{} took {}", $msg, format::time(::DurationExt::to_nanos(elapsed) as f64));

        out
    })
}

mod compare;

// Common analysis procedure
pub(crate) fn common<T>(
    id: &BenchmarkId,
    routine: &mut Routine<T>,
    config: &BenchmarkConfig,
    criterion: &Criterion,
    report_context: &ReportContext,
    parameter: &T,
    throughput: Option<Throughput>,
) {
    criterion.report.benchmark_start(id, report_context);

    let (iters, times, raw_metrics) =
        routine.sample(id, config, criterion, report_context, parameter);

    criterion.report.analysis(id, report_context);

    rename_new_dir_to_base(id.id(), &criterion.output_directory);

    let avg_times = iters
        .iter()
        .zip(times.iter())
        .map(|(&iters, &elapsed)| elapsed / iters)
        .collect::<Vec<f64>>();
    let avg_times = Sample::new(&avg_times);

    log_if_err!(fs::mkdirp(&format!(
        "{}/{}/new",
        criterion.output_directory, id
    )));

    let data = Data::new(&iters, &times);
    let labeled_sample = outliers(id, &criterion.output_directory, avg_times);
    let (distribution, slope) = regression(data, config);
    let (mut distributions, mut estimates) = estimates(avg_times, config);

    estimates.insert(Statistic::Slope, slope);
    distributions.insert(Statistic::Slope, distribution);

    log_if_err!(fs::save(
        &(data.x().as_slice(), data.y().as_slice()),
        &format!("{}/{}/new/sample.json", criterion.output_directory, id),
    ));
    log_if_err!(fs::save(
        &estimates,
        &format!("{}/{}/new/estimates.json", criterion.output_directory, id)
    ));

    let mut vals = BTreeMap::new();
    let mut avg_vals = BTreeMap::new();
    let mut metrics: BTreeMap<EventName, _> = BTreeMap::new();
    if let Some(ref m) = raw_metrics {
        for (name, mut values) in m {
            let values = values.into_iter().map(|&v| v as f64).collect::<Vec<f64>>();
            let avg_values = iters
                .iter()
                .zip(values.iter())
                .map(|(&i, &v)| v / i)
                .collect::<Vec<f64>>();

            // need to hoist ownership to parent stack frame in order to store
            // references to these
            vals.insert(name, values);
            avg_vals.insert(name, avg_values);
        }

        for (name, _) in m {
            let values = &vals[name];
            let avg_values = &avg_vals[name];

            let avg_values = Sample::new(avg_values.as_slice());

            let data = Data::new(&iters, &values);
            let labeled_sample = tukey::classify(avg_values);
            let (distribution, slope) = regression(data, config);
            let (mut distributions, mut absolute_estimates) = self::estimates(avg_values, config);

            absolute_estimates.insert(Statistic::Slope, slope);
            distributions.insert(Statistic::Slope, distribution);

            let measurement = ::report::MetricMeasurementData {
                sample: Sample::new(&values),
                avg: labeled_sample,
                absolute_estimates: absolute_estimates.clone(),
                distributions,
            };

            metrics.insert(name.clone(), measurement);
        }

        let estimates_to_write: BTreeMap<_, _> = metrics
            .iter()
            .map(|(name, measures)| (name, &measures.absolute_estimates))
            .collect();

        log_if_err!(fs::save(
            &estimates_to_write,
            &format!(
                "{}/{}/new/metrics-estimates.json",
                criterion.output_directory, id
            )
        ));
    };

    let compare_data = if base_dir_exists(id, &criterion.output_directory) {
        let result = compare::common(id, avg_times, config, criterion);
        match result {
            Ok((
                t_val,
                t_dist,
                rel_est,
                rel_dist,
                base_iters,
                base_times,
                base_avg,
                base_estimates,
            )) => {
                let p_value = t_dist.p_value(t_val, &Tails::Two);
                Some(::report::ComparisonData {
                    p_value: p_value,
                    t_distribution: t_dist,
                    t_value: t_val,
                    relative_estimates: rel_est,
                    relative_distributions: rel_dist,
                    significance_threshold: config.significance_level,
                    noise_threshold: config.noise_threshold,
                    base_iter_counts: base_iters,
                    base_sample_times: base_times,
                    base_avg_times: base_avg,
                    base_estimates: base_estimates,
                })
            }
            Err(e) => {
                ::error::log_error(&e);
                None
            }
        }
    } else {
        None
    };

    let measurement_data = ::report::MeasurementData {
        iter_counts: Sample::new(&*iters),
        sample_times: Sample::new(&*times),
        avg_times: labeled_sample,
        absolute_estimates: estimates.clone(),
        distributions: distributions,
        comparison: compare_data,
        throughput: throughput,
        metrics: if metrics.len() > 0 {
            Some(metrics)
        } else {
            None
        },
    };

    criterion
        .report
        .measurement_complete(id, report_context, &measurement_data);
}

fn base_dir_exists(id: &BenchmarkId, output_directory: &str) -> bool {
    Path::new(&format!("{}/{}/base", output_directory, id)).exists()
}

// Performs a simple linear regression on the sample
fn regression(data: Data<f64, f64>, config: &BenchmarkConfig) -> (Distribution<f64>, Estimate) {
    let cl = config.confidence_level;

    let distribution = elapsed!(
        "Bootstrapped linear regression",
        data.bootstrap(config.nresamples, |d| (Slope::fit(d).0,))
    ).0;

    let point = Slope::fit(data);
    let (lb, ub) = distribution.confidence_interval(config.confidence_level);
    let se = distribution.std_dev(None);

    (
        distribution,
        Estimate {
            confidence_interval: ConfidenceInterval {
                confidence_level: cl,
                lower_bound: lb,
                upper_bound: ub,
            },
            point_estimate: point.0,
            standard_error: se,
        },
    )
}

// Classifies the outliers in the sample
fn outliers<'a>(
    id: &BenchmarkId,
    output_directory: &str,
    avg_times: &'a Sample<f64>,
) -> LabeledSample<'a, f64> {
    let sample = tukey::classify(avg_times);
    log_if_err!(fs::save(
        &sample.fences(),
        &format!("{}/{}/new/tukey.json", output_directory, id)
    ));
    sample
}

// Estimates the statistics of the population from the sample
fn estimates(avg_times: &Sample<f64>, config: &BenchmarkConfig) -> (Distributions, Estimates) {
    fn stats(sample: &Sample<f64>) -> (f64, f64, f64, f64) {
        let mean = sample.mean();
        let std_dev = sample.std_dev(Some(mean));
        let median = sample.percentiles().median();
        let mad = sample.median_abs_dev(Some(median));

        (mean, std_dev, median, mad)
    }

    let cl = config.confidence_level;
    let nresamples = config.nresamples;

    let (mean, std_dev, median, mad) = stats(avg_times);
    let mut point_estimates = BTreeMap::new();
    point_estimates.insert(Statistic::Mean, mean);
    point_estimates.insert(Statistic::StdDev, std_dev);
    point_estimates.insert(Statistic::Median, median);
    point_estimates.insert(Statistic::MedianAbsDev, mad);

    let (dist_mean, dist_stddev, dist_median, dist_mad) = elapsed!(
        "Bootstrapping the absolute statistics.",
        avg_times.bootstrap(nresamples, stats)
    );

    let mut distributions = Distributions::new();
    distributions.insert(Statistic::Mean, dist_mean);
    distributions.insert(Statistic::StdDev, dist_stddev);
    distributions.insert(Statistic::Median, dist_median);
    distributions.insert(Statistic::MedianAbsDev, dist_mad);

    let estimates = Estimate::new(&distributions, &point_estimates, cl);

    (distributions, estimates)
}

fn rename_new_dir_to_base(id: &str, output_directory: &str) {
    let root_dir = Path::new(output_directory).join(id);
    let base_dir = root_dir.join("base");
    let new_dir = root_dir.join("new");

    if base_dir.exists() {
        try_else_return!(fs::rmrf(&base_dir));
    }
    if new_dir.exists() {
        try_else_return!(fs::mv(&new_dir, &base_dir));
    };
}
