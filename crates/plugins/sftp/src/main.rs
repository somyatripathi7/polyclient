fn main() -> anyhow::Result<()> {
    polyclient_plugin_runtime::run(
        polyclient_plugin_sftp::SftpPlugin::new(),
        polyclient_plugin_sftp::SftpDispatcher,
    )
}
