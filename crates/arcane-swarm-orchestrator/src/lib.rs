pub mod command_dispatcher;
pub mod driver_pool;
pub mod protocol;
pub mod server;
pub mod sse_server;
pub mod stats_collector;
pub mod telemetry;

#[cfg(test)]
mod tests {
    mod command_dispatch;
    mod dashboard_sse;
    mod driver_registration;
    mod stats_collector;
    mod telemetry_archive;
}
