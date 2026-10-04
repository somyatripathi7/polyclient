if [[ " $* " == *" --build "* ]]; then
    echo "The --build argument was passed!"
    pushd scripts
    ./dev-plugins.sh
    popd
fi

if [[ " $* " == *" --debug "* ]]; then
    echo "The --debug argument was passed!"
    export RUST_LOG=debug
fi


POLYCLIENT_PLUGINS_DIR=$(pwd)/dev-plugins npm run tauri dev