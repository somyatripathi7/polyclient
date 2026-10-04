fn main() -> anyhow::Result<()> {
    polyclient_plugin_runtime::run(
        polyclient_plugin_ssh::SshPlugin::new(),
        polyclient_plugin_ssh::SshDispatcher,
    )
}
