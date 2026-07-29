use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

/// ATT 当前构建的 Lua 版本。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    Lua54,
}
pub use self::Version::*;

/// 构建 Lua 静态库所需的目标和输出设置。
pub struct Build {
    out_dir: Option<PathBuf>,
    target: Option<String>,
    host: Option<String>,
    opt_level: Option<String>,
    debug: Option<bool>,
}

/// Lua 头文件、静态库和系统链接依赖。
#[derive(Clone, Debug)]
pub struct Artifacts {
    include_dir: PathBuf,
    lib_dir: PathBuf,
    libs: Vec<String>,
    system_libs: Vec<String>,
}

impl Default for Build {
    fn default() -> Self {
        Self {
            out_dir: env::var_os("OUT_DIR").map(PathBuf::from),
            target: env::var("TARGET").ok(),
            host: None,
            opt_level: None,
            debug: None,
        }
    }
}

impl Build {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn out_dir<P: AsRef<Path>>(&mut self, path: P) -> &mut Self {
        self.out_dir = Some(path.as_ref().to_path_buf());
        self
    }

    pub fn target(&mut self, target: &str) -> &mut Self {
        self.target = Some(target.to_owned());
        self
    }

    pub fn host(&mut self, host: &str) -> &mut Self {
        self.host = Some(host.to_owned());
        self
    }

    pub fn opt_level(&mut self, opt_level: &str) -> &mut Self {
        self.opt_level = Some(opt_level.to_owned());
        self
    }

    pub fn debug(&mut self, debug: bool) -> &mut Self {
        self.debug = Some(debug);
        self
    }

    pub fn build(&self, version: Version) -> Artifacts {
        self.try_build(version)
            .unwrap_or_else(|error| panic!("{error}"))
    }

    pub fn try_build(&self, _version: Version) -> Result<Artifacts, Box<dyn Error>> {
        let target = self.target.as_ref().ok_or("TARGET is not set")?;
        if target != "x86_64-pc-windows-msvc" {
            return Err(format!(
                "ATT vendored Lua only supports Windows MSVC targets, got '{target}'"
            )
            .into());
        }

        let out_dir = self.out_dir.as_ref().ok_or("OUT_DIR is not set")?;
        let source_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("lua-5.4.8");
        let lib_dir = out_dir.join("lib");
        let include_dir = out_dir.join("include");
        let source_files = source_files(&source_dir)?;

        for path in &source_files {
            println!("cargo:rerun-if-changed={}", path.display());
        }
        fs::create_dir_all(&include_dir)
            .context(|| format!("Cannot create '{}'", include_dir.display()))?;

        let mut config = cc::Build::new();
        config
            .warnings(false)
            .cargo_metadata(false)
            .target(target)
            .define("_AMD64_", None)
            .define("LUA_COMPAT_5_3", None)
            .include(&source_dir)
            .flag_if_supported("/utf-8")
            .flag_if_supported("-fno-common")
            .out_dir(&lib_dir);
        for path in source_files
            .iter()
            .filter(|path| path.extension().is_some_and(|extension| extension == "c"))
        {
            config.file(path);
        }

        match &self.host {
            Some(host) => {
                config.host(host);
            }
            None if env::var("HOST").is_ok() => {}
            None => {
                config.host(target);
            }
        }

        #[cfg(feature = "ucid")]
        config.define("LUA_UCID", None);

        let debug = self.debug.unwrap_or(cfg!(debug_assertions));
        if debug {
            config.define("LUA_USE_APICHECK", None).debug(true);
        }

        match &self.opt_level {
            Some(opt_level) => {
                config.opt_level_str(opt_level);
            }
            None if env::var("OPT_LEVEL").is_ok() => {}
            None => {
                config.opt_level(if debug { 0 } else { 2 });
            }
        }

        config.try_compile("lua5.4")?;

        for filename in ["lauxlib.h", "lua.h", "luaconf.h", "lualib.h"] {
            let from = source_dir.join(filename);
            let to = include_dir.join(filename);
            fs::copy(&from, &to)
                .context(|| format!("Cannot copy '{}' to '{}'", from.display(), to.display()))?;
        }

        Ok(Artifacts {
            include_dir,
            lib_dir,
            libs: vec!["lua5.4".to_owned()],
            system_libs: vec!["Pathcch".to_owned()],
        })
    }
}

impl Artifacts {
    pub fn include_dir(&self) -> &Path {
        &self.include_dir
    }

    pub fn lib_dir(&self) -> &Path {
        &self.lib_dir
    }

    pub fn libs(&self) -> &[String] {
        &self.libs
    }

    pub fn print_cargo_metadata(&self) {
        println!("cargo:rustc-link-search=native={}", self.lib_dir.display());
        for lib in &self.libs {
            println!("cargo:rustc-link-lib=static:-bundle={lib}");
        }
        for lib in &self.system_libs {
            println!("cargo:rustc-link-lib={lib}");
        }
        println!("cargo:include={}", self.include_dir.display());
        println!("cargo:lib={}", self.lib_dir.display());
    }
}

fn source_files(source_dir: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(source_dir)
        .context(|| format!("Cannot read '{}'", source_dir.display()))?
    {
        let entry =
            entry.context(|| format!("Cannot enumerate '{}'", source_dir.display()))?;
        if entry
            .file_type()
            .context(|| format!("Cannot inspect '{}'", entry.path().display()))?
            .is_file()
        {
            files.push(entry.path());
        }
    }
    files.sort_unstable();
    Ok(files)
}

trait ErrorContext<T> {
    fn context(self, f: impl FnOnce() -> String) -> Result<T, Box<dyn Error>>;
}

impl<T, E: Error> ErrorContext<T> for Result<T, E> {
    fn context(self, f: impl FnOnce() -> String) -> Result<T, Box<dyn Error>> {
        self.map_err(|error| format!("{}: {error}", f()).into())
    }
}
