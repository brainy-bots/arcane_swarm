pub mod command_dispatcher;
pub mod driver_pool;
pub mod protocol;
pub mod server;
pub mod stats_collector;

#[cfg(test)]
mod tests {
    mod command_dispatch;
    mod dashboard_sse;
    mod driver_registration;
    mod stats_collector;
    mod telemetry_archive;
}
