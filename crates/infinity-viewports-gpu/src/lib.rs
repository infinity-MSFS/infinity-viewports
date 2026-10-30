pub mod capture;
pub mod cloud;
pub mod hook;
pub mod stream;

/// The GPU capture subsystem. Holds the hook state.
pub struct Capturer {
    installed: bool,
}

impl Capturer {
    pub fn new() -> Capturer {
        Capturer { installed: false }
    }

    /// Install the D3D12/DXGI vtable-slot hooks, including the cloud-merge
    /// redirect (`CreateRenderTargetView`, `Reset`, `OMSetRenderTargets`).
    /// Only slot swaps, never byte patches. Returns a description of what was
    /// hooked, or a reason string on failure.
    pub fn install(&mut self) -> Result<String, String> {
        let desc = hook::install_present()?;
        self.installed = hook::is_installed();
        Ok(desc)
    }

    pub fn is_installed(&self) -> bool {
        self.installed
    }

    /// Restore every hooked vtable slot to the sim's originals and mark the hooks
    /// uninstalled. Call `stop_stream` first; drain a few presents before
    /// unloading the module. Reinstallable afterwards via `install`.
    pub fn uninstall(&mut self) -> String {
        let desc = hook::uninstall_present();
        self.installed = hook::is_installed();
        desc
    }

    /// What the `Present` hook has observed (present count, backbuffer size and
    /// format). Zeroed until the sim presents its first frame after the hook.
    pub fn observed(&self) -> hook::Observed {
        hook::observed()
    }

    /// The distinct render-target / depth-stencil resources seen at creation or
    /// via live barriers — our in-process capture of the render-target signatures.
    pub fn rt_inventory(&self) -> Vec<hook::RtEntry> {
        hook::rt_inventory()
    }

    /// `ResourceBarrier` calls observed (0 ⇒ sim uses a path we don't hook yet).
    pub fn resource_barrier_calls(&self) -> u64 {
        hook::resource_barrier_calls()
    }

    /// Request a one-shot scan of every readable render target at `width`×
    /// `height` in one frame — a diagnostic for identifying which resources of a
    /// viewport (e.g. the nose's 1024×576 set) carry colour / normals / material.
    pub fn request_layer_scan(&self, path: String, width: u32, height: u32) {
        capture::request_layer_scan(path, width, height);
    }

    /// Start continuous streaming of a viewport (depth + a selected colour RT)
    /// into the `VideoBlock` at `video_ptr`, for the live tuner. The mapping must
    /// outlive the stream.
    pub fn start_stream(&self, video_ptr: usize, width: u32, height: u32) {
        stream::start_stream(video_ptr, width, height);
    }

    /// Stop the continuous stream.
    pub fn stop_stream(&self) {
        stream::stop_stream();
    }

    /// Select which colour RT the stream copies (DXGI format + occurrence).
    pub fn set_stream_color_source(&self, format: u32, occurrence: u32) {
        stream::set_color_source(format, occurrence);
    }

    /// Whether the continuous stream is running.
    pub fn stream_active(&self) -> bool {
        stream::stream_active()
    }

    /// Frames the stream has published (diagnostics).
    pub fn stream_published_frames(&self) -> u64 {
        stream::published_frames()
    }

    /// Take the last layer scan's result message, if ready.
    pub fn take_dump_result(&self) -> Option<String> {
        capture::take_result()
    }
}

impl Default for Capturer {
    fn default() -> Self {
        Self::new()
    }
}
