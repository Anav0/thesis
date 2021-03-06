use crate::Context;
use color_eyre::{eyre::WrapErr, Report};
use tracing::instrument;
use tracing_futures::Instrument;
use tsunami::providers::aws;
use tsunami::Tsunami;

/// lobsters-noria; requires two machines: a client and a server
#[instrument(name = "lobsters-noria", skip(ctx))]
pub(crate) async fn main(ctx: Context) -> Result<(), Report> {
    crate::explore!(
        [
            (0, false, 0, false),
            // (0, true, 0, false),
            (0, true, 128 * 1024 * 1024, false),
            (0, true, 256 * 1024 * 1024, false),
            (0, true, 384 * 1024 * 1024, false),
            // (0, true, 0, true),
            (0, true, 128 * 1024 * 1024, true),
            (0, true, 256 * 1024 * 1024, true),
            (0, false, 0, true),
        ],
        one,
        ctx,
        false
    )
}

#[instrument(err, skip(ctx))]
pub(crate) async fn one(
    parameters: (usize, bool, usize, bool),
    loads: Option<Vec<usize>>,
    mut ctx: Context,
) -> Result<usize, Report> {
    let (nshards, partial, memlimit, mut durable) = parameters;
    let mut last_good_scale = 0;

    let mut aws = crate::launcher();
    aws.set_mode(aws::LaunchMode::on_demand());

    // try to ensure we do AWS cleanup
    let result: Result<_, Report> = try {
        tracing::info!("spinning up aws instances");

        aws.spawn(
            vec![
                (
                    String::from("server"),
                    aws::Setup::default()
                        .instance_type(&ctx.server_type)
                        .ami(crate::AMI, "ubuntu")
                        .availability_zone(ctx.az.clone())
                        .setup(crate::noria_setup("noria-server", "noria-server")),
                ),
                (
                    String::from("client"),
                    aws::Setup::default()
                        .instance_type(&ctx.client_type)
                        .ami(crate::AMI, "ubuntu")
                        .availability_zone(ctx.az.clone())
                        .setup(crate::noria_setup("noria-applications", "lobsters-noria")),
                ),
            ],
            None,
        )
        .await
        .wrap_err("failed to start instances")?;

        tracing::debug!("connecting");
        let vms = aws.connect_all().await?;
        let server = vms.get("server").unwrap();
        let client = vms.get("client").unwrap();
        let s = &server.ssh;
        let c = &client.ssh;
        tracing::debug!("connected");

        if durable {
            tracing::debug!("mount ramdisk");
            crate::output_on_success(s.shell("sudo mount -t tmpfs -o size=60G tmpfs /mnt"))
                .await
                .wrap_err("mount ramdisk")?;
        }

        let mut scales = if let Some(loads) = loads {
            Box::new(cliff::LoadIterator::from(loads)) as Box<dyn cliff::CliffSearch + Send>
        } else if durable && partial {
            // we don't normally run non-durable partial @ 6k scale, so run that too (6001)
            Box::new(cliff::LoadIterator::from(vec![2000, 6000, 6001]))
                as Box<dyn cliff::CliffSearch + Send>
        } else if durable {
            Box::new(cliff::LoadIterator::from(vec![6000])) as Box<dyn cliff::CliffSearch + Send>
        } else {
            Box::new(cliff::ExponentialCliffSearcher::until(2000, 250))
        };
        let result: Result<(), Report> = try {
            let mut successful_scale = None;
            while let Some(mut scale) = scales.next() {
                if scale % 2 == 1 {
                    tracing::warn!(%scale, "switching to non-durable");
                    assert!(durable);
                    scale -= 1;
                    durable = false;
                }

                if let Some(scale) = successful_scale.take() {
                    // last run succeeded at the given scale
                    last_good_scale = scale;
                }
                successful_scale = Some(scale);

                if !partial && !durable && nshards == 0 && scale >= 6_250 {
                    // this runs out of memory
                    scales.overloaded();
                    tracing::warn!(%scale, "skipping full scale that runs out of memory");
                    continue;
                }
                if partial && nshards == 0 && scale >= 11_000 {
                    // this runs partial out of memory
                    scales.overloaded();
                    tracing::warn!(%scale, "skipping partial scale that runs out of memory");
                    continue;
                }

                if *ctx.exit.borrow() {
                    tracing::info!("exiting as instructed");
                    break;
                }

                let scale_span = tracing::info_span!("scale", scale);
                async {
                    tracing::info!("start benchmark target");
                    let mut backend = if nshards == 0 {
                        "direct".to_string()
                    } else {
                        format!("direct_{}", nshards)
                    };
                    if !partial {
                        backend.push_str("_full");
                    }
                    if durable {
                        backend.push_str("_durable");
                    }
                    let prefix = format!("lobsters-{}-{}-{}m", backend, scale, memlimit);

                    if durable {
                        tracing::debug!("remount ramdisk");
                        crate::output_on_success(s.shell("sudo umount /mnt"))
                            .await
                            .wrap_err("unmount ramdisk")?;
                        crate::output_on_success(
                            s.shell("sudo mount -t tmpfs -o size=60G tmpfs /mnt"),
                        )
                        .await
                        .wrap_err("remount ramdisk")?;
                    }

                    tracing::trace!("starting noria server");
                    let dir = if durable { Some("/mnt") } else { None };
                    let mut noria_server = crate::server::build(s, server, dir);
                    if !partial {
                        noria_server.arg("--no-partial");
                    }
                    let durability = if durable {
                        "--durability=persistent"
                    } else {
                        "--durability=memory"
                    };
                    let noria_server = noria_server
                        .arg(durability)
                        .arg("--no-reuse")
                        .arg("--shards")
                        .arg(nshards.to_string())
                        .arg("-m")
                        .arg(memlimit.to_string())
                        .spawn()
                        .wrap_err("failed to start noria-server")?;

                    crate::invoke::lobsters::run(
                        &prefix,
                        scale,
                        || {
                            scales.overloaded();
                            successful_scale.take();
                        },
                        c,
                        &server,
                        crate::invoke::lobsters::Backend::Noria,
                        &mut ctx,
                    )
                    .await?;

                    if !*ctx.exit.borrow() {
                        tracing::debug!("stopping server");
                        crate::server::stop(s, noria_server).await?;
                        tracing::trace!("server stopped");
                    }

                    Ok::<_, Report>(())
                }
                .instrument(scale_span)
                .await?;
            }
        };

        tracing::debug!("cleaning up");
        tracing::trace!("cleaning up ssh connections");
        for (name, host) in vms {
            let host_span = tracing::trace_span!("ssh_close", name = &*name);
            async {
                tracing::trace!("closing connection");
                if let Err(e) = host.ssh.close().await {
                    tracing::warn!("ssh connection failed: {}", e);
                }
            }
            .instrument(host_span)
            .await
        }

        result?
    };

    tracing::trace!("cleaning up instances");
    let cleanup = aws.terminate_all().await;
    tracing::debug!("done");
    let _ = result?;
    let _ = cleanup.wrap_err("cleanup failed")?;
    Ok(last_good_scale)
}
