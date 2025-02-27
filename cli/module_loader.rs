// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

use crate::args::CliOptions;
use crate::args::DenoSubcommand;
use crate::args::TsTypeLib;
use crate::cache::ModuleInfoCache;
use crate::cache::ParsedSourceCache;
use crate::emit::Emitter;
use crate::graph_util::graph_lock_or_exit;
use crate::graph_util::graph_valid_with_cli_options;
use crate::graph_util::workspace_config_to_workspace_members;
use crate::graph_util::DenoGraphFsAdapter;
use crate::graph_util::FileWatcherReporter;
use crate::graph_util::ModuleGraphBuilder;
use crate::graph_util::ModuleGraphContainer;
use crate::node;
use crate::resolver::CliGraphResolver;
use crate::resolver::CliNodeResolver;
use crate::resolver::ModuleCodeStringSource;
use crate::resolver::NpmModuleLoader;
use crate::tools::check;
use crate::tools::check::TypeChecker;
use crate::util::progress_bar::ProgressBar;
use crate::util::text_encoding::code_without_source_map;
use crate::util::text_encoding::source_map_from_code;
use crate::worker::ModuleLoaderFactory;

use deno_ast::MediaType;
use deno_core::anyhow::anyhow;
use deno_core::anyhow::Context;
use deno_core::error::custom_error;
use deno_core::error::generic_error;
use deno_core::error::AnyError;
use deno_core::futures::future::FutureExt;
use deno_core::futures::Future;
use deno_core::parking_lot::Mutex;
use deno_core::resolve_url;
use deno_core::resolve_url_or_path;
use deno_core::ModuleCodeString;
use deno_core::ModuleLoader;
use deno_core::ModuleSource;
use deno_core::ModuleSourceCode;
use deno_core::ModuleSpecifier;
use deno_core::ModuleType;
use deno_core::RequestedModuleType;
use deno_core::ResolutionKind;
use deno_core::SourceMapGetter;
use deno_graph::source::ResolutionMode;
use deno_graph::source::Resolver;
use deno_graph::EsmModule;
use deno_graph::JsonModule;
use deno_graph::Module;
use deno_graph::Resolution;
use deno_lockfile::Lockfile;
use deno_runtime::colors;
use deno_runtime::deno_fs;
use deno_runtime::permissions::PermissionsContainer;
use deno_semver::npm::NpmPackageReqReference;
use std::borrow::Cow;
use std::collections::HashSet;
use std::pin::Pin;
use std::rc::Rc;
use std::str;
use std::sync::Arc;

pub struct ModuleLoadPreparer {
  options: Arc<CliOptions>,
  fs: Arc<dyn deno_fs::FileSystem>,
  graph_container: Arc<ModuleGraphContainer>,
  lockfile: Option<Arc<Mutex<Lockfile>>>,
  maybe_file_watcher_reporter: Option<FileWatcherReporter>,
  module_graph_builder: Arc<ModuleGraphBuilder>,
  module_info_cache: Arc<ModuleInfoCache>,
  parsed_source_cache: Arc<ParsedSourceCache>,
  progress_bar: ProgressBar,
  resolver: Arc<CliGraphResolver>,
  type_checker: Arc<TypeChecker>,
}

impl ModuleLoadPreparer {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    options: Arc<CliOptions>,
    fs: Arc<dyn deno_fs::FileSystem>,
    graph_container: Arc<ModuleGraphContainer>,
    lockfile: Option<Arc<Mutex<Lockfile>>>,
    maybe_file_watcher_reporter: Option<FileWatcherReporter>,
    module_graph_builder: Arc<ModuleGraphBuilder>,
    module_info_cache: Arc<ModuleInfoCache>,
    parsed_source_cache: Arc<ParsedSourceCache>,
    progress_bar: ProgressBar,
    resolver: Arc<CliGraphResolver>,
    type_checker: Arc<TypeChecker>,
  ) -> Self {
    Self {
      options,
      fs,
      graph_container,
      lockfile,
      maybe_file_watcher_reporter,
      module_graph_builder,
      module_info_cache,
      parsed_source_cache,
      progress_bar,
      resolver,
      type_checker,
    }
  }

  /// This method must be called for a module or a static importer of that
  /// module before attempting to `load()` it from a `JsRuntime`. It will
  /// populate the graph data in memory with the necessary source code, write
  /// emits where necessary or report any module graph / type checking errors.
  #[allow(clippy::too_many_arguments)]
  pub async fn prepare_module_load(
    &self,
    roots: Vec<ModuleSpecifier>,
    is_dynamic: bool,
    lib: TsTypeLib,
    permissions: PermissionsContainer,
  ) -> Result<(), AnyError> {
    log::debug!("Preparing module load.");
    let _pb_clear_guard = self.progress_bar.clear_guard();

    let mut cache = self.module_graph_builder.create_fetch_cacher(permissions);
    let maybe_imports = self.options.to_maybe_imports()?;
    let maybe_workspace_config = self.options.maybe_workspace_config();
    let workspace_members = if let Some(wc) = maybe_workspace_config {
      workspace_config_to_workspace_members(wc)?
    } else {
      vec![]
    };
    let graph_resolver = self.resolver.as_graph_resolver();
    let graph_npm_resolver = self.resolver.as_graph_npm_resolver();
    let maybe_file_watcher_reporter = self
      .maybe_file_watcher_reporter
      .as_ref()
      .map(|r| r.as_reporter());

    let parser = self.parsed_source_cache.as_capturing_parser();
    let analyzer = self.module_info_cache.as_module_analyzer(&parser);

    log::debug!("Creating module graph.");
    let mut graph_update_permit =
      self.graph_container.acquire_update_permit().await;
    let graph = graph_update_permit.graph_mut();

    // Determine any modules that have already been emitted this session and
    // should be skipped.
    let reload_exclusions: HashSet<ModuleSpecifier> =
      graph.specifiers().map(|(s, _)| s.clone()).collect();

    self
      .module_graph_builder
      .build_graph_with_npm_resolution(
        graph,
        roots.clone(),
        &mut cache,
        deno_graph::BuildOptions {
          is_dynamic,
          imports: maybe_imports,
          file_system: Some(&DenoGraphFsAdapter(self.fs.as_ref())),
          resolver: Some(graph_resolver),
          npm_resolver: Some(graph_npm_resolver),
          module_parser: Some(&parser),
          module_analyzer: Some(&analyzer),
          reporter: maybe_file_watcher_reporter,
          workspace_fast_check: false,
          workspace_members,
        },
      )
      .await?;

    graph_valid_with_cli_options(
      graph,
      self.fs.as_ref(),
      &roots,
      &self.options,
    )?;

    // If there is a lockfile...
    if let Some(lockfile) = &self.lockfile {
      let mut lockfile = lockfile.lock();
      // validate the integrity of all the modules
      graph_lock_or_exit(graph, &mut lockfile);
      // update it with anything new
      lockfile.write().context("Failed writing lockfile.")?;
    }

    // save the graph and get a reference to the new graph
    let graph = graph_update_permit.commit();

    drop(_pb_clear_guard);

    // type check if necessary
    if self.options.type_check_mode().is_true()
      && !self.graph_container.is_type_checked(&roots, lib)
    {
      let graph = Arc::new(graph.segment(&roots));
      self
        .type_checker
        .check(
          graph,
          check::CheckOptions {
            lib,
            log_ignored_options: false,
            reload: self.options.reload_flag()
              && !roots.iter().all(|r| reload_exclusions.contains(r)),
          },
        )
        .await?;
      self.graph_container.set_type_checked(&roots, lib);
    }

    log::debug!("Prepared module load.");

    Ok(())
  }

  /// Helper around prepare_module_load that loads and type checks
  /// the provided files.
  pub async fn load_and_type_check_files(
    &self,
    files: &[String],
  ) -> Result<(), AnyError> {
    let lib = self.options.ts_type_lib_window();

    let specifiers = self.collect_specifiers(files)?;

    if specifiers.is_empty() {
      log::warn!("{} No matching files found.", colors::yellow("Warning"));
    }

    self
      .prepare_module_load(
        specifiers,
        false,
        lib,
        PermissionsContainer::allow_all(),
      )
      .await
  }

  fn collect_specifiers(
    &self,
    files: &[String],
  ) -> Result<Vec<ModuleSpecifier>, AnyError> {
    let excludes = self.options.resolve_config_excludes()?;
    Ok(
      files
        .iter()
        .filter_map(|file| {
          let file_url =
            resolve_url_or_path(file, self.options.initial_cwd()).ok()?;
          if file_url.scheme() != "file" {
            return Some(file_url);
          }
          // ignore local files that match any of files listed in `exclude` option
          let file_path = file_url.to_file_path().ok()?;
          if excludes.matches_path(&file_path) {
            None
          } else {
            Some(file_url)
          }
        })
        .collect::<Vec<_>>(),
    )
  }
}

struct PreparedModuleLoader {
  emitter: Arc<Emitter>,
  graph_container: Arc<ModuleGraphContainer>,
  parsed_source_cache: Arc<ParsedSourceCache>,
}

impl PreparedModuleLoader {
  pub fn load_prepared_module(
    &self,
    specifier: &ModuleSpecifier,
    maybe_referrer: Option<&ModuleSpecifier>,
  ) -> Result<ModuleCodeStringSource, AnyError> {
    if specifier.scheme() == "node" {
      unreachable!(); // Node built-in modules should be handled internally.
    }

    let graph = self.graph_container.graph();
    match graph.get(specifier) {
      Some(deno_graph::Module::Json(JsonModule {
        source,
        media_type,
        specifier,
        ..
      })) => Ok(ModuleCodeStringSource {
        code: source.clone().into(),
        found_url: specifier.clone(),
        media_type: *media_type,
      }),
      Some(deno_graph::Module::Esm(EsmModule {
        source,
        media_type,
        specifier,
        ..
      })) => {
        let code: ModuleCodeString = match media_type {
          MediaType::JavaScript
          | MediaType::Unknown
          | MediaType::Cjs
          | MediaType::Mjs
          | MediaType::Json => source.clone().into(),
          MediaType::Dts | MediaType::Dcts | MediaType::Dmts => {
            Default::default()
          }
          MediaType::TypeScript
          | MediaType::Mts
          | MediaType::Cts
          | MediaType::Jsx
          | MediaType::Tsx => {
            // get emit text
            self
              .emitter
              .emit_parsed_source(specifier, *media_type, source)?
          }
          MediaType::TsBuildInfo | MediaType::Wasm | MediaType::SourceMap => {
            panic!("Unexpected media type {media_type} for {specifier}")
          }
        };

        // at this point, we no longer need the parsed source in memory, so free it
        self.parsed_source_cache.free(specifier);

        Ok(ModuleCodeStringSource {
          code,
          found_url: specifier.clone(),
          media_type: *media_type,
        })
      }
      _ => {
        let mut msg = format!("Loading unprepared module: {specifier}");
        if let Some(referrer) = maybe_referrer {
          msg = format!("{}, imported from: {}", msg, referrer.as_str());
        }
        Err(anyhow!(msg))
      }
    }
  }
}

struct SharedCliModuleLoaderState {
  lib_window: TsTypeLib,
  lib_worker: TsTypeLib,
  is_inspecting: bool,
  is_repl: bool,
  graph_container: Arc<ModuleGraphContainer>,
  module_load_preparer: Arc<ModuleLoadPreparer>,
  prepared_module_loader: PreparedModuleLoader,
  resolver: Arc<CliGraphResolver>,
  node_resolver: Arc<CliNodeResolver>,
  npm_module_loader: NpmModuleLoader,
}

pub struct CliModuleLoaderFactory {
  shared: Arc<SharedCliModuleLoaderState>,
}

impl CliModuleLoaderFactory {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    options: &CliOptions,
    emitter: Arc<Emitter>,
    graph_container: Arc<ModuleGraphContainer>,
    module_load_preparer: Arc<ModuleLoadPreparer>,
    parsed_source_cache: Arc<ParsedSourceCache>,
    resolver: Arc<CliGraphResolver>,
    node_resolver: Arc<CliNodeResolver>,
    npm_module_loader: NpmModuleLoader,
  ) -> Self {
    Self {
      shared: Arc::new(SharedCliModuleLoaderState {
        lib_window: options.ts_type_lib_window(),
        lib_worker: options.ts_type_lib_worker(),
        is_inspecting: options.is_inspecting(),
        is_repl: matches!(
          options.sub_command(),
          DenoSubcommand::Repl(_) | DenoSubcommand::Jupyter(_)
        ),
        prepared_module_loader: PreparedModuleLoader {
          emitter,
          graph_container: graph_container.clone(),
          parsed_source_cache,
        },
        graph_container,
        module_load_preparer,
        resolver,
        node_resolver,
        npm_module_loader,
      }),
    }
  }

  fn create_with_lib(
    &self,
    lib: TsTypeLib,
    root_permissions: PermissionsContainer,
    dynamic_permissions: PermissionsContainer,
  ) -> Rc<dyn ModuleLoader> {
    Rc::new(CliModuleLoader {
      lib,
      root_permissions,
      dynamic_permissions,
      shared: self.shared.clone(),
    })
  }
}

impl ModuleLoaderFactory for CliModuleLoaderFactory {
  fn create_for_main(
    &self,
    root_permissions: PermissionsContainer,
    dynamic_permissions: PermissionsContainer,
  ) -> Rc<dyn ModuleLoader> {
    self.create_with_lib(
      self.shared.lib_window,
      root_permissions,
      dynamic_permissions,
    )
  }

  fn create_for_worker(
    &self,
    root_permissions: PermissionsContainer,
    dynamic_permissions: PermissionsContainer,
  ) -> Rc<dyn ModuleLoader> {
    self.create_with_lib(
      self.shared.lib_worker,
      root_permissions,
      dynamic_permissions,
    )
  }

  fn create_source_map_getter(&self) -> Option<Box<dyn SourceMapGetter>> {
    Some(Box::new(CliSourceMapGetter {
      shared: self.shared.clone(),
    }))
  }
}

struct CliModuleLoader {
  lib: TsTypeLib,
  /// The initial set of permissions used to resolve the static imports in the
  /// worker. These are "allow all" for main worker, and parent thread
  /// permissions for Web Worker.
  root_permissions: PermissionsContainer,
  /// Permissions used to resolve dynamic imports, these get passed as
  /// "root permissions" for Web Worker.
  dynamic_permissions: PermissionsContainer,
  shared: Arc<SharedCliModuleLoaderState>,
}

impl CliModuleLoader {
  fn load_sync(
    &self,
    specifier: &ModuleSpecifier,
    maybe_referrer: Option<&ModuleSpecifier>,
    is_dynamic: bool,
    requested_module_type: RequestedModuleType,
  ) -> Result<ModuleSource, AnyError> {
    let permissions = if is_dynamic {
      &self.dynamic_permissions
    } else {
      &self.root_permissions
    };
    let code_source = if let Some(result) = self
      .shared
      .npm_module_loader
      .load_sync_if_in_npm_package(specifier, maybe_referrer, permissions)
    {
      result?
    } else {
      self
        .shared
        .prepared_module_loader
        .load_prepared_module(specifier, maybe_referrer)?
    };
    let code = if self.shared.is_inspecting {
      // we need the code with the source map in order for
      // it to work with --inspect or --inspect-brk
      code_source.code
    } else {
      // reduce memory and throw away the source map
      // because we don't need it
      code_without_source_map(code_source.code)
    };
    let module_type = match code_source.media_type {
      MediaType::Json => ModuleType::Json,
      _ => ModuleType::JavaScript,
    };

    // If we loaded a JSON file, but the "requested_module_type" (that is computed from
    // import attributes) is not JSON we need to fail.
    if module_type == ModuleType::Json
      && requested_module_type != RequestedModuleType::Json
    {
      return Err(generic_error("Attempted to load JSON module without specifying \"type\": \"json\" attribute in the import statement."));
    }

    Ok(ModuleSource::new_with_redirect(
      module_type,
      ModuleSourceCode::String(code),
      specifier,
      &code_source.found_url,
    ))
  }
}

impl ModuleLoader for CliModuleLoader {
  fn resolve(
    &self,
    specifier: &str,
    referrer: &str,
    kind: ResolutionKind,
  ) -> Result<ModuleSpecifier, AnyError> {
    let permissions = if matches!(kind, ResolutionKind::DynamicImport) {
      &self.dynamic_permissions
    } else {
      &self.root_permissions
    };

    // TODO(bartlomieju): ideally we shouldn't need to call `current_dir()` on each
    // call - maybe it should be caller's responsibility to pass it as an arg?
    let cwd = std::env::current_dir().context("Unable to get CWD")?;
    let referrer_result = deno_core::resolve_url_or_path(referrer, &cwd);

    if let Ok(referrer) = referrer_result.as_ref() {
      if let Some(result) = self.shared.node_resolver.resolve_if_in_npm_package(
        specifier,
        referrer,
        permissions,
      ) {
        return result;
      }

      let graph = self.shared.graph_container.graph();
      let maybe_resolved = match graph.get(referrer) {
        Some(Module::Esm(module)) => {
          module.dependencies.get(specifier).map(|d| &d.maybe_code)
        }
        _ => None,
      };

      match maybe_resolved {
        Some(Resolution::Ok(resolved)) => {
          let specifier = &resolved.specifier;

          return match graph.get(specifier) {
            Some(Module::Npm(module)) => {
              let package_folder = self
                .shared
                .node_resolver
                .npm_resolver
                .as_managed()
                .unwrap() // byonm won't create a Module::Npm
                .resolve_pkg_folder_from_deno_module(
                  module.nv_reference.nv(),
                )?;
              self
                .shared
                .node_resolver
                .resolve_package_sub_path(
                  &package_folder,
                  module.nv_reference.sub_path(),
                  referrer,
                  permissions,
                )
                .with_context(|| {
                  format!("Could not resolve '{}'.", module.nv_reference)
                })
            }
            Some(Module::Node(module)) => Ok(module.specifier.clone()),
            Some(Module::Esm(module)) => Ok(module.specifier.clone()),
            Some(Module::Json(module)) => Ok(module.specifier.clone()),
            Some(Module::External(module)) => {
              Ok(node::resolve_specifier_into_node_modules(&module.specifier))
            }
            None => Ok(specifier.clone()),
          };
        }
        Some(Resolution::Err(err)) => {
          return Err(custom_error(
            "TypeError",
            format!("{}\n", err.to_string_with_range()),
          ))
        }
        Some(Resolution::None) | None => {}
      }
    }

    // FIXME(bartlomieju): this is a hacky way to provide compatibility with REPL
    // and `Deno.core.evalContext` API. Ideally we should always have a referrer filled
    // but sadly that's not the case due to missing APIs in V8.
    let referrer = if referrer.is_empty() && self.shared.is_repl {
      deno_core::resolve_path("./$deno$repl.ts", &cwd)?
    } else {
      referrer_result?
    };

    // FIXME(bartlomieju): this is another hack way to provide NPM specifier
    // support in REPL. This should be fixed.
    let resolution = self.shared.resolver.resolve(
      specifier,
      &deno_graph::Range {
        specifier: referrer.clone(),
        start: deno_graph::Position::zeroed(),
        end: deno_graph::Position::zeroed(),
      },
      ResolutionMode::Execution,
    );

    if self.shared.is_repl {
      let specifier = resolution
        .as_ref()
        .ok()
        .map(Cow::Borrowed)
        .or_else(|| ModuleSpecifier::parse(specifier).ok().map(Cow::Owned));
      if let Some(specifier) = specifier {
        if let Ok(reference) =
          NpmPackageReqReference::from_specifier(&specifier)
        {
          return self.shared.node_resolver.resolve_req_reference(
            &reference,
            permissions,
            &referrer,
          );
        }
      }
    }

    resolution.map_err(|err| err.into())
  }

  fn load(
    &self,
    specifier: &ModuleSpecifier,
    maybe_referrer: Option<&ModuleSpecifier>,
    is_dynamic: bool,
    requested_module_type: RequestedModuleType,
  ) -> deno_core::ModuleLoadResponse {
    // NOTE: this block is async only because of `deno_core` interface
    // requirements; module was already loaded when constructing module graph
    // during call to `prepare_load` so we can load it synchronously.
    deno_core::ModuleLoadResponse::Sync(self.load_sync(
      specifier,
      maybe_referrer,
      is_dynamic,
      requested_module_type,
    ))
  }

  fn prepare_load(
    &self,
    specifier: &ModuleSpecifier,
    _maybe_referrer: Option<String>,
    is_dynamic: bool,
  ) -> Pin<Box<dyn Future<Output = Result<(), AnyError>>>> {
    if let Some(result) =
      self.shared.npm_module_loader.maybe_prepare_load(specifier)
    {
      return Box::pin(deno_core::futures::future::ready(result));
    }

    let specifier = specifier.clone();
    let module_load_preparer = self.shared.module_load_preparer.clone();

    let root_permissions = if is_dynamic {
      self.dynamic_permissions.clone()
    } else {
      self.root_permissions.clone()
    };
    let lib = self.lib;

    async move {
      module_load_preparer
        .prepare_module_load(vec![specifier], is_dynamic, lib, root_permissions)
        .await
    }
    .boxed_local()
  }
}

struct CliSourceMapGetter {
  shared: Arc<SharedCliModuleLoaderState>,
}

impl SourceMapGetter for CliSourceMapGetter {
  fn get_source_map(&self, file_name: &str) -> Option<Vec<u8>> {
    let specifier = resolve_url(file_name).ok()?;
    match specifier.scheme() {
      // we should only be looking for emits for schemes that denote external
      // modules, which the disk_cache supports
      "wasm" | "file" | "http" | "https" | "data" | "blob" => (),
      _ => return None,
    }
    let source = self
      .shared
      .prepared_module_loader
      .load_prepared_module(&specifier, None)
      .ok()?;
    source_map_from_code(&source.code)
  }

  fn get_source_line(
    &self,
    file_name: &str,
    line_number: usize,
  ) -> Option<String> {
    let graph = self.shared.graph_container.graph();
    let code = match graph.get(&resolve_url(file_name).ok()?) {
      Some(deno_graph::Module::Esm(module)) => &module.source,
      Some(deno_graph::Module::Json(module)) => &module.source,
      _ => return None,
    };
    // Do NOT use .lines(): it skips the terminating empty line.
    // (due to internally using_terminator() instead of .split())
    let lines: Vec<&str> = code.split('\n').collect();
    if line_number >= lines.len() {
      Some(format!(
        "{} Couldn't format source line: Line {} is out of bounds (source may have changed at runtime)",
        crate::colors::yellow("Warning"), line_number + 1,
      ))
    } else {
      Some(lines[line_number].to_string())
    }
  }
}
