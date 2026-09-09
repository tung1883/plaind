fn main() {
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon_with_id("assets/plaind.ico", "APPICON");
        if let Err(e) = res.compile() {
            println!("cargo:warning=icon resource not embedded: {e}");
        }
    }
}
