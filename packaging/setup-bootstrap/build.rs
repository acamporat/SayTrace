fn main() {
    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("../../src-tauri/icons/icon.ico");
        resource
            .compile()
            .expect("unable to attach the SayTrace setup icon");
    }
}
