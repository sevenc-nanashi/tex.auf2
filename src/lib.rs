use std::hash::{Hash, Hasher};

use aviutl2::{
    anyhow::{self, Context},
    filter::{FilterConfigItemSliceExt, FilterConfigItems},
    tracing,
};

#[aviutl2::plugin(GenericPlugin)]
struct TexAuf2 {
    filter: aviutl2::generic::SubPlugin<TexFilter>,
}

impl aviutl2::generic::GenericPlugin for TexAuf2 {
    fn new(info: aviutl2::AviUtl2Info) -> aviutl2::AnyResult<Self> {
        Ok(Self {
            filter: aviutl2::generic::SubPlugin::new_filter_plugin(&info)?,
        })
    }

    fn plugin_info(&self) -> aviutl2::generic::GenericPluginTable {
        aviutl2::generic::GenericPluginTable {
            name: "tex.auf2".to_string(),
            information: format!(
                "Render TeX as filter objects / v{} / https://github.com/sevenc-nanashi/tex.auf2",
                env!("CARGO_PKG_VERSION")
            ),
        }
    }

    fn register(&mut self, registry: &mut aviutl2::generic::HostAppHandle) {
        registry.register_filter_plugin(&self.filter);
    }

    fn on_clear_cache(&mut self, _edit_section: &aviutl2::generic::EditSection) {
        tracing::info!("Clearing render cache");
        RENDER_CACHES.clear();
    }
}

#[derive(Clone, Default, PartialEq)]
struct TexCacheKey {
    tex: String,
    font_size: f32,
    color: u32,
}
impl std::hash::Hash for TexCacheKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.tex.hash(state);
        self.font_size.to_bits().hash(state);
        self.color.hash(state);
    }
}

#[derive(Clone, Default, educe::Educe)]
#[educe(Debug, PartialEq)]
struct TexCacheEntry {
    buffer: Vec<u8>,
    width: u32,
    height: u32,
}

static RENDER_CACHES: std::sync::LazyLock<dashmap::DashMap<u64, TexCacheEntry>> =
    std::sync::LazyLock::new(dashmap::DashMap::new);

static FONT_DB: std::sync::LazyLock<std::sync::Arc<resvg::usvg::fontdb::Database>> =
    std::sync::LazyLock::new(|| {
        let mut db = resvg::usvg::fontdb::Database::new();
        db.load_system_fonts();
        std::sync::Arc::new(db)
    });

#[aviutl2::plugin(FilterPlugin)]
struct TexFilter {}

#[aviutl2::filter::filter_config_items]
struct TexConfig {
    #[track(name = "サイズ", step = 0.01, default = 100.0, range = 1..=1000.0)]
    font_size: f32,
    #[color(name = "色", default = 0xffffff)]
    color: u32,
    #[text(name = "TeX")]
    tex: String,

    #[group(name = "高度な設定", opened = false)]
    _advanced: group! {
        #[check(name = "キャッシュを使用", default = true)]
        use_cache: bool,
    },
}

impl aviutl2::filter::FilterPlugin for TexFilter {
    fn new(_info: aviutl2::AviUtl2Info) -> aviutl2::AnyResult<Self> {
        aviutl2::tracing_subscriber::fmt()
            .with_max_level(if cfg!(debug_assertions) {
                tracing::Level::DEBUG
            } else {
                tracing::Level::INFO
            })
            .event_format(aviutl2::logger::AviUtl2Formatter)
            .with_writer(aviutl2::logger::AviUtl2LogWriter)
            .init();
        Ok(Self {})
    }

    fn plugin_info(&self) -> aviutl2::filter::FilterPluginTable {
        aviutl2::filter::FilterPluginTable {
            name: "tex.auf2".to_string(),
            label: None,
            flags: aviutl2::bitflag!(aviutl2::filter::FilterPluginFlags {
                video: true,
                as_object: true
            }),
            information: format!(
                "Render TeX as filter objects / v{} / https://github.com/sevenc-nanashi/tex.auf2",
                env!("CARGO_PKG_VERSION")
            ),
            config_items: TexConfig::to_config_items(),
        }
    }

    fn proc_video(
        &self,
        config: &[aviutl2::filter::FilterConfigItem],
        video: &mut aviutl2::filter::FilterProcVideo,
    ) -> aviutl2::AnyResult<()> {
        let config = config.to_struct::<TexConfig>();

        let cache_key = TexCacheKey {
            tex: config.tex.clone(),
            font_size: config.font_size,
            color: config.color,
        };
        let cache_key_hash = {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            cache_key.hash(&mut hasher);
            hasher.finish()
        };
        if config.use_cache {
            let cache_entry = RENDER_CACHES
                .entry(cache_key_hash)
                .or_try_insert_with(|| {
                    tracing::info!("Cache miss for key {cache_key_hash}, rendering TeX");
                    let svg_data = render_tex(&cache_key).context("Failed to render TeX")?;
                    tracing::info!(
                        "Rendered TeX: {} bytes, dimensions: {}x{}",
                        svg_data.buffer.len(),
                        svg_data.width,
                        svg_data.height
                    );
                    anyhow::Ok(TexCacheEntry {
                        buffer: svg_data.buffer,
                        width: svg_data.width,
                        height: svg_data.height,
                    })
                })
                .map_err(|e| anyhow::anyhow!("Failed to acquire cache entry: {e:?}"))?;

            video.set_image_data(&cache_entry.buffer, cache_entry.width, cache_entry.height);
        } else {
            let cache_entry = render_tex(&cache_key).context("Failed to render TeX")?;
            video.set_image_data(&cache_entry.buffer, cache_entry.width, cache_entry.height);
        }
        Ok(())
    }
}

fn render_tex(key: &TexCacheKey) -> anyhow::Result<TexCacheEntry> {
    use resvg::usvg::{Options, Tree};

    let options = Options {
        fontdb: FONT_DB.clone(),
        font_size: key.font_size,
        style_sheet: Some(format!("svg {{ color: #{:06x}; }}", key.color)),
        ..Default::default()
    };

    tracing::debug!("Rendering TeX: {:?}", key.tex);
    let svg = mathjax_svg_rs::render_tex_with_font_size(&key.tex, key.font_size as f64)
        .map_err(|e| anyhow::anyhow!("Failed to render TeX: {e}"))?;
    tracing::debug!("Generated SVG: {} bytes", svg.len());
    let tree = Tree::from_str(&svg, &options).context("Failed to parse TeX as SVG")?;
    let pixmap_size = tree.size();
    let mut pixmap = resvg::tiny_skia::Pixmap::new(
        (pixmap_size.width()).ceil() as u32,
        (pixmap_size.height()).ceil() as u32,
    )
    .context("Failed to create pixmap")?;
    resvg::render(&tree, Default::default(), &mut pixmap.as_mut());

    Ok(TexCacheEntry {
        buffer: pixmap.data().to_vec(),
        width: pixmap.width(),
        height: pixmap.height(),
    })
}

aviutl2::register_generic_plugin!(TexAuf2);
