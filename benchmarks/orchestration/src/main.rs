#![feature(try_blocks, label_break_value)]

const AMI: &str = "ami-0a8091371c4d91a3e";

use clap::{App, Arg};
use color_eyre::{eyre, eyre::WrapErr, Report};
use std::future::Future;
use std::pin::Pin;
use tracing::instrument;
use tracing_futures::Instrument;
use tsunami::providers::aws;

#[derive(Debug, Clone)]
struct Context {
    server_type: String,
    client_type: String,
    exit: tokio::sync::watch::Receiver<bool>,
    az: aws::AvailabilityZoneSpec,
}

#[macro_export]
macro_rules! explore {
    ([$($arg:expr),+,], $one:ident, $ctx:ident, $min:expr) => {{
        crate::explore!([$($arg),*], $one, $ctx, $min)
    }};
    ([$($arg:expr),*], $one:ident, $ctx:ident, $min:expr) => {{
        use tokio::stream::StreamExt;

        let targets = vec![$($arg),*];
        let mut futs = futures_util::stream::futures_unordered::FuturesUnordered::new();
        let mut results = Vec::new();
        for (i, target) in targets.iter().enumerate() {
            results.push(Ok(0));

            if futs.len() >= 3 {
                // don't overwhelm ec2
                let (i, r) = futs.next().await.expect(".len() > 0");
                if let Err(ref e) = r {
                    eprintln!("{:?}", e);
                }
                results[i] = r;
            }

            let mut ctx = $ctx.clone();
            // we need to await exit so that it only yields again when we should exit
            // we need to do this for _every_ clone of exit
            if let Some(false) = ctx.exit.recv().await {
            } else {
                tracing::info!("exiting early as instructed");
                break;
            }

            let fut = tokio::spawn($one(target.clone(), None, ctx).in_current_span());
            futs.push(async move {
                (i, fut.await.expect("runtime went away?"))
            });

            // don't overwhelm ec2 by issuing all the requests at once
            tokio::time::delay_for(std::time::Duration::from_secs(10)).await;
        }

        tracing::debug!("waiting for experiments to finish");

        // collect the remaining results
        if !futs.is_empty() {
            while let Some((i, r)) = futs.next().await {
                if let Err(ref e) = r {
                    eprintln!("{:?}", e);
                }
                results[i] = r;
            }
        }

        tracing::info!("all experiments finished");
        Ok(())
    }};

    (@IT $tup:ident, $head:expr, $n:expr) => {
        $tup.$n
    };

    (@IT $tup:ident, $head:expr; $($tail:expr);+, $n:expr) => {
        $tup.$n, crate::explore!(@IT $tup, $($tail);+, $n + 1)
    };
}

mod lobsters_mysql;
mod lobsters_noria;
mod lobsters_noria_mem;
mod vote;
mod vote_hybrid;
mod vote_mem;
mod vote_migration;
mod vote_redis;

mod invoke;

pub(crate) mod server;

#[tokio::main]
async fn main() {
    let mut benchmarks = vec![
        "vote-redis",
        // "vote-hybrid",
        "vote-migration",
        "vote-memory",
        "lobsters-mysql",
        "lobsters-noria",
        "lobsters-noria-memory",
        "vote",
    ];

    let matches = App::new("Noria benchmark orchestrator")
        .author("Jon Gjengset <jon@tsp.io>")
        .about("Run Noria benchmarks on EC2")
        .arg(
            Arg::with_name("benchmarks")
                .index(1)
                .multiple(true)
                .possible_values(&benchmarks)
                .help("Run only the specified benchmarks [all by default]"),
        )
        .arg(
            Arg::with_name("server")
                .long("server-instance")
                .default_value("r5n.4xlarge")
                .help("Run the noria server on an instance of this type"),
        )
        .arg(
            Arg::with_name("az")
                .long("az")
                .takes_value(true)
                .possible_values(&[
                    "us-east-1a",
                    "us-east-1b",
                    "us-east-1c",
                    "us-east-1d",
                    "us-east-1e",
                    "us-east-1f",
                ])
                .help("Spin up instances in the given availability zone"),
        )
        .arg(
            Arg::with_name("client")
                .long("client-instance")
                .default_value("m5n.4xlarge")
                .help("Run the benchmark clients on instances of this type"),
        )
        .get_matches();

    // only run specified benchmarks
    if let Some(vs) = matches.values_of("benchmarks") {
        benchmarks.clear();
        benchmarks.extend(vs);
    }

    let server_type = matches
        .value_of("server")
        .expect("has default value")
        .to_string();
    let client_type = matches
        .value_of("client")
        .expect("has default value")
        .to_string();

    // set up tracing
    use tracing_error::ErrorLayer;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{fmt, EnvFilter};
    let fmt_layer = fmt::layer().with_target(false);
    let filter_layer = EnvFilter::from_default_env();
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer)
        .with(ErrorLayer::default())
        .init();

    // set up a mechanism for stopping the program early
    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for ^c");
        tracing::info!("exit signal received");
        let _ = tx.broadcast(true);
    });

    // wrap all the contextual benchmark info in a Context
    let az = if let Some(az) = matches.value_of("az") {
        aws::AvailabilityZoneSpec::Specify(String::from(az))
    } else {
        aws::AvailabilityZoneSpec::Any
    };
    let ctx = Context {
        server_type,
        client_type,
        exit: rx,
        az,
    };

    tracing::info!("running benchmarks");
    for benchmark in benchmarks {
        if *ctx.exit.borrow() {
            tracing::warn!("exiting early as instructed");
        }

        let result = match benchmark {
            "vote-migration" => vote_migration::main(ctx.clone()).await,
            "vote" => vote::main(ctx.clone()).await,
            "vote-memory" => vote_mem::main(ctx.clone()).await,
            "vote-redis" => vote_redis::main(ctx.clone()).await,
            "vote-hybrid" => vote_hybrid::main(ctx.clone()).await,
            "lobsters-mysql" => lobsters_mysql::main(ctx.clone()).await,
            "lobsters-noria" => lobsters_noria::main(ctx.clone()).await,
            "lobsters-noria-memory" => lobsters_noria_mem::main(ctx.clone()).await,
            _ => unreachable!("{}", benchmark),
        };

        if let Err(ref e) = result {
            tracing::error!(%benchmark, "benchmark failed: {}", e);
            eprintln!("{:?}", e);
        } else {
            tracing::debug!(%benchmark, "benchmark completed");
        }
    }
    tracing::info!("all benchmarks completed");
}

fn launcher() -> aws::Launcher<rusoto_sts::StsAssumeRoleSessionCredentialsProvider> {
    aws::Launcher::default().with_credentials(|| {
        let sts = rusoto_sts::StsClient::new(rusoto_core::Region::UsEast1);
        Ok(rusoto_sts::StsAssumeRoleSessionCredentialsProvider::new(
            sts,
            "arn:aws:sts::125163634912:role/soup".to_owned(),
            "jon-thesis".to_owned(),
            None,
            None,
            None,
            None,
        ))
    })
}

/// Prepare a box to run a particular experiment.
///
/// Note that we _generate_ a setup function, so that the setup can differ per experiment.
#[instrument(level = "debug")]
fn noria_setup(
    _package: &'static str,
    binary: &'static str,
) -> Box<
    dyn for<'r> Fn(
            &'r tsunami::Machine<'_>,
        ) -> Pin<Box<dyn Future<Output = Result<(), Report>> + Send + 'r>>
        + Send
        + Sync
        + 'static,
> {
    Box::new(move |vm| {
        Box::pin(
            async move {
                // first, make sure we have the latest release
                tracing::debug!("setting up host");
                tracing::trace!("git pull");
                let updated = vm
                    .ssh
                    .shell("git -C noria pull")
                    .status()
                    .await
                    .wrap_err("git pull")?;
                if !updated.success() {
                    eyre::bail!("git pull failed");
                }

                // then, we need to compile the target binary
                tracing::trace!("cargo build");
                let compiled = vm
                    .ssh
                    .shell(format!(
                        "cd noria && cargo build --bin {} --release",
                        binary
                    ))
                    .output()
                    .await
                    .wrap_err("cargo build")?;
                if !compiled.status.success() {
                    return Err(
                        eyre::eyre!(String::from_utf8_lossy(&compiled.stderr).to_string())
                            .wrap_err("failed to compile"),
                    );
                }

                if binary == "noria-server" {
                    // and then ensure that ZooKeeper is running
                    tracing::trace!("start zookeeper");
                    let zk = vm
                        .ssh
                        .shell("sudo systemctl start zookeeper")
                        .status()
                        .await
                        .wrap_err("start zookeeper")?;
                    if !zk.success() {
                        eyre::bail!("failed to start zookeeper")
                    }
                }

                tracing::debug!("setup complete");
                Ok(())
            }
            .in_current_span(),
        )
    })
}

fn noria_bin<'s>(ssh: &'s openssh::Session, binary: &'static str) -> openssh::Command<'s> {
    let mut cmd = ssh.command("env");
    cmd.arg("RUST_BACKTRACE=1")
        .arg(format!("target/release/{}", binary));
    cmd
}

#[instrument(level = "debug", skip(cmd))]
async fn output_on_success<'a, C: std::borrow::BorrowMut<openssh::Command<'a>>>(
    mut cmd: C,
) -> Result<(Vec<u8>, Vec<u8>), Report> {
    let proc = cmd
        .borrow_mut()
        .output()
        .await
        .wrap_err("failed to execute")?;
    if proc.status.success() {
        Ok((proc.stdout, proc.stderr))
    } else {
        let code = proc.status.code().unwrap_or(0);
        if proc.stderr.is_empty() {
            Err(eyre::eyre!(
                "process exited with {} with empty stderr",
                code
            ))
        } else {
            Err(
                eyre::eyre!(String::from_utf8_lossy(&proc.stderr).to_string())
                    .wrap_err(format!("process exited with {}", code)),
            )
        }
    }
}

#[instrument(level = "debug", skip(ssh))]
pub(crate) async fn noria_commit(ssh: &openssh::Session) -> Result<String, Report> {
    let commit = crate::output_on_success(
        ssh.command("git")
            .arg("-C")
            .arg("noria")
            .arg("rev-parse")
            .arg("HEAD"),
    )
    .await
    .wrap_err("git")?;

    Ok(String::from_utf8_lossy(&commit.0).trim().to_string())
}

#[instrument(level = "debug", skip(ssh))]
pub(crate) async fn load(ssh: &openssh::Session) -> Result<(f64, f64), Report> {
    let load = crate::output_on_success(
        ssh.command("awk")
            .arg("{print $1\" \"$2}")
            .arg("/proc/loadavg"),
    )
    .await
    .wrap_err("awk")?;

    let load = String::from_utf8_lossy(&load.0);

    let mut loads = load
        .split_whitespace()
        .map(|c| -> Result<f64, _> { c.parse() });

    if let Some(Ok(load1)) = loads.next() {
        if let Some(Ok(load5)) = loads.next() {
            return Ok((load1, load5));
        }
    }

    Err(eyre::eyre!(load.to_string())).wrap_err("bad load")
}
