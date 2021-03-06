use crate::Context;
use color_eyre::{eyre::WrapErr, Report};
use tracing::instrument;
use tracing_futures::Instrument;
use tsunami::providers::aws;
use tsunami::Tsunami;

const KB: usize = 1024;
const MB: usize = 1024 * KB;

/// vote_mem; requires at least two machines: a server and 1+ clients
#[instrument(name = "vote-mem", skip(ctx))]
pub(crate) async fn main(ctx: Context) -> Result<(), Report> {
    crate::explore!([(250_000, 100, "skewed", 4)], one, ctx, true)
}

#[instrument(err, skip(ctx))]
pub(crate) async fn one(
    parameters: (usize, usize, &'static str, usize),
    limits: Option<Vec<usize>>,
    mut ctx: Context,
) -> Result<usize, Report> {
    let (target, write_every, distribution, nclients) = parameters;
    let partial = true;
    let mut last_good_limit = 0;

    let mut aws = crate::launcher();
    aws.set_mode(aws::LaunchMode::on_demand());

    // try to ensure we do AWS cleanup
    let result: Result<_, Report> = try {
        tracing::info!("spinning up aws instances");
        let mut instances = vec![(
            String::from("server"),
            aws::Setup::default()
                .instance_type(&ctx.server_type)
                .ami(crate::AMI, "ubuntu")
                .availability_zone(ctx.az.clone())
                .setup(crate::noria_setup("noria-server", "noria-server")),
        )];
        for clienti in 0..nclients {
            instances.push((
                format!("client{}", clienti),
                aws::Setup::default()
                    .instance_type(&ctx.client_type)
                    .ami(crate::AMI, "ubuntu")
                    .availability_zone(ctx.az.clone())
                    .setup(crate::noria_setup("noria-applications", "vote")),
            ));
        }
        aws.spawn(instances, None)
            .await
            .wrap_err("failed to start instances")?;

        tracing::debug!("connecting");
        let vms = aws.connect_all().await?;
        let server = vms.get("server").unwrap();
        let s = &server.ssh;
        let cs: Vec<_> = (0..nclients)
            .map(|clienti| &vms.get(&format!("client{}", clienti)).unwrap().ssh)
            .collect();
        tracing::debug!("connected");

        let mut limits = if let Some(limits) = limits {
            Box::new(cliff::LoadIterator::from(limits)) as Box<dyn cliff::CliffSearch + Send>
        } else {
            Box::new(cliff::BinaryMinSearcher::until(512 * MB, 4 * MB))
                as Box<dyn cliff::CliffSearch + Send>
        };
        let mut zero = Some(0);
        let result: Result<(), Report> = try {
            let mut successful_limit = None;
            while let Some(limit) = zero.take().or_else(|| limits.next()) {
                if let Some(limit) = successful_limit.take() {
                    // last run succeeded at the given limit
                    last_good_limit = limit;
                }
                successful_limit = Some(limit);

                if limit == 0 && target % 250_000 == 0 && (target / 250_000).is_power_of_two() {
                    // we already have this
                    tracing::info!(%target, "skipping non-limited target we already have");
                    continue;
                }

                if *ctx.exit.borrow() {
                    tracing::info!("exiting as instructed");
                    break;
                }

                let limit_span = tracing::info_span!("limit", limit);
                async {
                    tracing::info!("start benchmark target");
                    let backend = if partial { "partial" } else { "full" };
                    let prefix = format!(
                        "{}.10000000a.{}t.{}r.{}c.{}m.{}",
                        backend, target, write_every, nclients, limit, distribution,
                    );

                    tracing::trace!("starting noria server");
                    let mut noria_server = crate::server::build(s, server, None);
                    if !partial {
                        noria_server.arg("--no-partial");
                    }
                    let noria_server = noria_server
                        .arg("--durability=memory")
                        .arg("--no-reuse")
                        .arg("--shards=0")
                        .arg("-m")
                        .arg(limit.to_string())
                        .spawn()
                        .wrap_err("failed to start noria-server")?;

                    crate::invoke::vote::run(
                        &prefix,
                        target,
                        distribution,
                        write_every,
                        || {
                            limits.overloaded();
                            successful_limit.take();
                        },
                        &cs[..],
                        &server,
                        crate::invoke::vote::Backend::Netsoup { join: true },
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
                .instrument(limit_span)
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
                    tracing::warn!("ssh connection failed: {:?}", e);
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
    Ok(last_good_limit)
}
