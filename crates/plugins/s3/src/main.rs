fn main() -> anyhow::Result<()> {
    polyclient_plugin_runtime::run(
        polyclient_plugin_s3::S3Plugin::new(),
        polyclient_plugin_s3::S3Dispatcher,
    )
}
