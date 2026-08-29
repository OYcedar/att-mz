//! 严格 TOML 配置边界。
//!
//! 原始 TOML 只在本模块存在。结构和字段类型全部通过后，本模块继续建立路径基准、
//! 语言模块、LLM Client 外部约束与 Profile 唯一性；业务和根适配器只接收受信配置。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::num::{NonZeroU32, NonZeroUsize};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};
use toml_parser::decoder::ScalarKind;
use toml_parser::parser::{Event, EventKind, ValidateWhitespace, parse_document};
use toml_parser::{ParseError, Source, Span};
use url::Url;
use zeroize::Zeroizing;

use super::arguments::{
    DataExportArguments, DataExportCommand, ExtractArguments, GenericCommand, GenericInitArguments,
    InitArguments, ManualArguments, ManualCommand, ManualExportArguments, ManualSelectionArgument,
    MvCommand, MzCommand, ProductCommand, ProjectLuaArguments, RpgMakerManualCommand,
    TranslateArguments, WriteBackArguments,
};

use crate::diagnostic::{
    ConfigurationTomlFailureKind, ConfigurationTomlValueKind, ConfigurationValueRule,
};
use crate::language::{
    EnglishLanguageModule, EnglishResidualPolicy, EnglishTranslationDetectionPolicy,
    JapaneseLanguageModule, JapaneseResidualPolicy, LanguageId, LanguageIdError, LanguageModule,
    LanguageModuleCatalog, LanguageModuleCatalogBuildError, LanguagePolicyConfigurationError,
};
use crate::llm::ApiKeyRedactor;
use crate::manual::{ManualExportSelection, ManualOperation};
use crate::project_name::ProjectName;
use crate::rpg_maker::RpgMakerLayout;
use crate::rpg_maker::extract::document::RpgMakerDocumentReadingConfig;
use crate::runtime::cpu::CpuExecutorConfig;
use crate::runtime::filesystem::{DirectoryPublisherConfig, SystemFileSystemConfig};
use crate::runtime::llm::{
    LlmProxyConfiguration, OpenAiCompatibleClient, OpenAiEndpoint, OpenAiExecutorConfiguration,
    OpenAiProtocol,
};
use crate::runtime::sqlite::RusqliteStorageConfiguration;
use crate::translation::profile::TranslationRequestConfiguration;

const CHAT_COMPLETIONS_RESERVED_REQUEST_BODY_FIELDS: [&str; 3] = ["model", "messages", "stream"];
const RESPONSES_RESERVED_REQUEST_BODY_FIELDS: [&str; 4] =
    ["model", "input", "stream", "background"];
const CONFIGURATION_FILE_NAME: &str = "config.toml";
const PROJECTS_DIRECTORY_NAME: &str = "projects";
const PROMPTS_DIRECTORY_NAME: &str = "prompts";

/// 由实际运行的可执行文件唯一确定的发行目录布局。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DistributionLayout {
    configuration_path: PathBuf,
    projects_root: PathBuf,
    prompts_root: PathBuf,
}

impl DistributionLayout {
    pub(crate) fn from_current_executable() -> Result<Self, DistributionLayoutError> {
        let executable_path =
            std::env::current_exe().map_err(DistributionLayoutError::CurrentExecutable)?;
        Self::from_executable_path(executable_path)
    }

    fn from_executable_path(executable_path: PathBuf) -> Result<Self, DistributionLayoutError> {
        let root = executable_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| DistributionLayoutError::ExecutableDirectoryMissing {
                path: executable_path.clone(),
            })?
            .to_path_buf();
        Ok(Self {
            configuration_path: root.join(CONFIGURATION_FILE_NAME),
            projects_root: root.join(PROJECTS_DIRECTORY_NAME),
            prompts_root: root.join(PROMPTS_DIRECTORY_NAME),
        })
    }

    #[cfg(test)]
    fn for_test_configuration(configuration_path: &Path) -> Self {
        let root = configuration_path
            .parent()
            .expect("测试配置必须拥有父目录")
            .to_path_buf();
        Self {
            configuration_path: configuration_path.to_path_buf(),
            projects_root: root.join(PROJECTS_DIRECTORY_NAME),
            prompts_root: root.join(PROMPTS_DIRECTORY_NAME),
        }
    }

    pub(crate) fn configuration_path(&self) -> &Path {
        &self.configuration_path
    }

    pub(crate) fn projects_root(&self) -> &Path {
        &self.projects_root
    }

    pub(crate) fn prompts_root(&self) -> &Path {
        &self.prompts_root
    }
}

/// 读取配置，并且只建立本次命令实际消费的受信配置。
pub(crate) fn load_product_configuration(
    distribution: &DistributionLayout,
    product: ProductCommand,
) -> Result<ConfiguredProductCommand, ConfigurationLoadError> {
    let configuration_path = distribution.configuration_path().to_path_buf();
    let mut file =
        File::open(&configuration_path).map_err(|source| ConfigurationLoadError::Open {
            path: configuration_path.clone(),
            source,
        })?;
    let metadata = file
        .metadata()
        .map_err(|source| ConfigurationLoadError::Read {
            path: configuration_path.clone(),
            source,
        })?;
    if !metadata.is_file() {
        return Err(ConfigurationLoadError::NotAFile {
            path: configuration_path,
        });
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.by_ref()
        .read_to_end(&mut bytes)
        .map_err(|source| ConfigurationLoadError::Read {
            path: configuration_path.clone(),
            source,
        })?;

    let source = std::str::from_utf8(bytes.as_slice()).map_err(|source| {
        ConfigurationLoadError::InvalidUtf8 {
            path: configuration_path.clone(),
            valid_up_to: source.valid_up_to(),
            error_len: source.error_len(),
        }
    })?;
    let toml_index = Arc::new(ConfigurationTomlIndex::build(source, &configuration_path)?);
    toml_index.validate_complete_field_set(source, &configuration_path)?;
    match product {
        ProductCommand::Test => {
            ConfiguredTestCommand::build(&configuration_path, source, toml_index.as_ref())
                .map(ConfiguredProductCommand::Test)
        }
        ProductCommand::Mz { command } => ConfiguredRpgMakerCommand::build(
            &configuration_path,
            distribution,
            source,
            toml_index,
            RpgMakerLayout::MZ,
            RpgMakerCommandArguments::from(command),
            None,
        )
        .map(|command| ConfiguredProductCommand::RpgMaker {
            layout: RpgMakerLayout::MZ,
            command,
        }),
        ProductCommand::Mv { command } => {
            let (command, dialogue_rules_path) = normalize_mv_command(command);
            ConfiguredRpgMakerCommand::build(
                &configuration_path,
                distribution,
                source,
                toml_index,
                RpgMakerLayout::MV,
                command,
                dialogue_rules_path,
            )
            .map(|command| ConfiguredProductCommand::RpgMaker {
                layout: RpgMakerLayout::MV,
                command,
            })
        }
        ProductCommand::Generic { command } => ConfiguredGenericCommand::build(
            &configuration_path,
            distribution,
            source,
            toml_index,
            command,
        )
        .map(ConfiguredProductCommand::Generic),
    }
    .map_err(|error| error.with_configuration_path(&configuration_path))
}

#[cfg(test)]
fn load_configuration(
    requested_path: &Path,
    command: MzCommand,
) -> Result<ConfiguredRpgMakerCommand, ConfigurationLoadError> {
    let distribution = DistributionLayout::for_test_configuration(requested_path);
    load_product_configuration(&distribution, ProductCommand::Mz { command }).map(|configured| {
        match configured {
            ConfiguredProductCommand::RpgMaker { command, .. } => command,
            ConfiguredProductCommand::Test(_) => unreachable!("测试传入 MZ 命令"),
            ConfiguredProductCommand::Generic(_) => unreachable!("测试传入 MZ 命令"),
        }
    })
}

pub(crate) enum ConfiguredProductCommand {
    Test(ConfiguredTestCommand),
    RpgMaker {
        layout: RpgMakerLayout,
        command: ConfiguredRpgMakerCommand,
    },
    Generic(ConfiguredGenericCommand),
}

/// 根测试命令已经完成严格配置校验后的全部唯一 Client。
pub(crate) struct ConfiguredTestCommand {
    clients: Vec<ConfiguredTestClient>,
}

impl ConfiguredTestCommand {
    fn build(
        configuration_path: &Path,
        source: &str,
        toml_index: &ConfigurationTomlIndex,
    ) -> Result<Self, ConfigurationLoadError> {
        let raw: RawTestSelection = parse_selected(
            source,
            configuration_path,
            toml_index,
            ConfigurationSelection::NoAdditionalFields,
        )?;
        let configuration_directory = configuration_path.parent().expect("配置文件必须拥有父目录");
        if raw.llm.clients.is_empty() {
            return Err(ConfigurationLoadError::InvalidValue(invalid(
                "llm.clients",
                ConfigurationValueRule::ValueBlank,
            )));
        }
        let mut clients = Vec::with_capacity(raw.llm.clients.len());
        for (id, raw_client) in raw.llm.clients {
            validate_exact_identifier("llm client id", &id)
                .map_err(ConfigurationLoadError::InvalidValue)?;
            let protocol = OpenAiProtocol::from(raw_client.protocol);
            let stream = raw_client.stream;
            let built = build_llm_client(
                format!("llm.clients.{id}").as_str(),
                configuration_directory,
                raw_client,
            )
            .map_err(ConfigurationLoadError::InvalidValue)?;
            clients.push(ConfiguredTestClient {
                id,
                protocol,
                stream,
                executor: built.executor,
                client: built.client,
            });
        }
        Ok(Self { clients })
    }

    pub(crate) fn clients(&self) -> &[ConfiguredTestClient] {
        &self.clients
    }
}

pub(crate) struct ConfiguredTestClient {
    id: String,
    protocol: OpenAiProtocol,
    stream: bool,
    executor: SelectedLlmExecutorConfiguration,
    client: OpenAiCompatibleClient,
}

impl ConfiguredTestClient {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) const fn protocol(&self) -> OpenAiProtocol {
        self.protocol
    }

    pub(crate) const fn stream(&self) -> bool {
        self.stream
    }

    pub(crate) const fn executor(&self) -> &SelectedLlmExecutorConfiguration {
        &self.executor
    }

    pub(crate) const fn client(&self) -> &OpenAiCompatibleClient {
        &self.client
    }

    pub(crate) fn api_key_redactor(&self) -> Arc<ApiKeyRedactor> {
        self.client.api_key_redactor()
    }
}

fn normalize_mv_command(command: MvCommand) -> (RpgMakerCommandArguments, Option<PathBuf>) {
    match command {
        MvCommand::Init(arguments) => (RpgMakerCommandArguments::Init(arguments), None),
        MvCommand::Extract(arguments) => (
            RpgMakerCommandArguments::Extract(ExtractArguments {
                project: arguments.project,
                builtin: arguments.builtin,
                rules: arguments.rules,
            }),
            arguments.dialogue_rules,
        ),
        MvCommand::Translate(arguments) => (RpgMakerCommandArguments::Translate(arguments), None),
        MvCommand::WriteBack(arguments) => (RpgMakerCommandArguments::WriteBack(arguments), None),
        MvCommand::Manual { command } => (RpgMakerCommandArguments::Manual(command), None),
        MvCommand::Ownership { command } => (RpgMakerCommandArguments::Ownership(command), None),
        MvCommand::Translation { command } => {
            (RpgMakerCommandArguments::Translation(command), None)
        }
        MvCommand::Lua(arguments) => (RpgMakerCommandArguments::Lua(arguments), None),
    }
}

enum RpgMakerCommandArguments {
    Init(InitArguments),
    Extract(ExtractArguments),
    Translate(TranslateArguments),
    WriteBack(WriteBackArguments),
    Manual(RpgMakerManualCommand),
    Ownership(DataExportCommand),
    Translation(DataExportCommand),
    Lua(ProjectLuaArguments),
}

impl From<MzCommand> for RpgMakerCommandArguments {
    fn from(command: MzCommand) -> Self {
        match command {
            MzCommand::Init(arguments) => Self::Init(arguments),
            MzCommand::Extract(arguments) => Self::Extract(arguments),
            MzCommand::Translate(arguments) => Self::Translate(arguments),
            MzCommand::WriteBack(arguments) => Self::WriteBack(arguments),
            MzCommand::Manual { command } => Self::Manual(command),
            MzCommand::Ownership { command } => Self::Ownership(command),
            MzCommand::Translation { command } => Self::Translation(command),
            MzCommand::Lua(arguments) => Self::Lua(arguments),
        }
    }
}

/// 五个互斥命令各自拥有且只拥有现实消费的配置。
pub(crate) enum ConfiguredRpgMakerCommand {
    Init(ConfiguredInitCommand),
    Extract(ConfiguredExtractCommand),
    Translate(Box<ConfiguredTranslateCommand>),
    WriteBack(ConfiguredWriteBackCommand),
    Manual(ConfiguredManualCommand),
    Ownership(ConfiguredManualCommand),
    Translation(ConfiguredManualCommand),
    Lua(ConfiguredProjectLuaCommand),
}

impl ConfiguredRpgMakerCommand {
    fn build(
        configuration_path: &Path,
        distribution: &DistributionLayout,
        source: &str,
        toml_index: Arc<ConfigurationTomlIndex>,
        layout: RpgMakerLayout,
        command: RpgMakerCommandArguments,
        dialogue_rules_path: Option<PathBuf>,
    ) -> Result<Self, ConfigurationLoadError> {
        let common = CommonCommandConfiguration::build(distribution.projects_root());

        match command {
            RpgMakerCommandArguments::Init(arguments) => {
                let _: RawInitSelection = parse_selected(
                    source,
                    configuration_path,
                    toml_index.as_ref(),
                    ConfigurationSelection::NoAdditionalFields,
                )?;
                let publisher = build_directory_publisher_configuration(
                    common.projects_root(),
                    layout.engine().storage_name(),
                );
                Ok(Self::Init(ConfiguredInitCommand {
                    arguments,
                    common,
                    publisher,
                }))
            }
            RpgMakerCommandArguments::Extract(arguments) => {
                let _: RawExtractSelection = parse_selected(
                    source,
                    configuration_path,
                    toml_index.as_ref(),
                    ConfigurationSelection::NoAdditionalFields,
                )?;
                let cpu = build_cpu_configuration();
                let ExtractArguments {
                    project,
                    builtin,
                    rules,
                } = arguments;
                let rpg_maker = ExtractConfiguration::build(builtin, rules);
                Ok(Self::Extract(ConfiguredExtractCommand {
                    project_name: project.name,
                    common,
                    cpu,
                    rpg_maker,
                    dialogue_rules_path,
                }))
            }
            RpgMakerCommandArguments::Translate(arguments) => {
                let deferred_source = Arc::new(DeferredConfigurationSource::new(
                    configuration_path,
                    source,
                    Arc::clone(&toml_index),
                ));
                let raw: RawTranslateSelection = parse_selected(
                    source,
                    configuration_path,
                    toml_index.as_ref(),
                    ConfigurationSelection::Translate,
                )?;
                let cpu = build_cpu_configuration();
                let record_translation_tasks = raw.translation.record_translation_tasks;
                let TranslateArguments {
                    project,
                    profile_id,
                    terms,
                    placeholders,
                    retry_rejected,
                } = arguments;
                let rpg_maker = PendingTranslateConfiguration::build(
                    distribution.prompts_root(),
                    raw.prompts,
                    raw.languages,
                    raw.translation,
                )
                .map_err(ConfigurationLoadError::InvalidValue)?;
                let configured = ConfiguredTranslateCommand {
                    project_name: project.name,
                    configuration_path: configuration_path.to_path_buf(),
                    terminology_path: terms,
                    placeholder_rules_path: placeholders,
                    common,
                    cpu,
                    record_translation_tasks,
                    retry_rejected,
                    profile: ConfiguredTranslateProfile::Deferred {
                        source: deferred_source,
                        configuration: rpg_maker,
                    },
                };
                let configured = match profile_id {
                    Some(profile_id) => configured.resolve_profile(&profile_id)?,
                    None => configured,
                };
                Ok(Self::Translate(Box::new(configured)))
            }
            RpgMakerCommandArguments::WriteBack(arguments) => {
                let raw: RawWriteBackSelection = parse_selected(
                    source,
                    configuration_path,
                    toml_index.as_ref(),
                    ConfigurationSelection::WriteBack,
                )?;
                let cpu = build_cpu_configuration();
                let publisher = build_directory_publisher_configuration(
                    common.projects_root(),
                    layout.engine().storage_name(),
                );
                let WriteBackArguments {
                    project,
                    layout_rules,
                } = arguments;
                let rpg_maker = WriteBackConfiguration::build(raw.write_back);
                Ok(Self::WriteBack(ConfiguredWriteBackCommand {
                    project_name: project.name,
                    layout_rules_path: layout_rules,
                    common,
                    cpu,
                    publisher,
                    rpg_maker,
                }))
            }
            RpgMakerCommandArguments::Manual(command) => {
                Ok(Self::Manual(ConfiguredManualCommand::build_rpg_maker(
                    configuration_path,
                    source,
                    toml_index.as_ref(),
                    command,
                    common,
                )?))
            }
            RpgMakerCommandArguments::Ownership(command) => {
                Ok(Self::Ownership(ConfiguredManualCommand::build_data_export(
                    configuration_path,
                    source,
                    toml_index.as_ref(),
                    command,
                    ManualOperation::OwnershipExport,
                    common,
                    false,
                )?))
            }
            RpgMakerCommandArguments::Translation(command) => Ok(Self::Translation(
                ConfiguredManualCommand::build_data_export(
                    configuration_path,
                    source,
                    toml_index.as_ref(),
                    command,
                    ManualOperation::TranslationExport,
                    common,
                    false,
                )?,
            )),
            RpgMakerCommandArguments::Lua(arguments) => {
                let raw: RawManualSelection = parse_selected(
                    source,
                    configuration_path,
                    toml_index.as_ref(),
                    ConfigurationSelection::Languages,
                )?;
                let language_modules = build_language_modules(raw.languages)
                    .map_err(ConfigurationLoadError::InvalidValue)?;
                let ProjectLuaArguments {
                    project,
                    script,
                    arguments,
                } = arguments;
                Ok(Self::Lua(ConfiguredProjectLuaCommand {
                    project_name: project.name,
                    common,
                    script: ConfiguredProjectLuaScript::new(script),
                    arguments,
                    language_modules,
                }))
            }
        }
    }
}

/// Generic 命令已经完成配置读取后的进程输入。
pub(crate) enum ConfiguredGenericCommand {
    Init {
        arguments: GenericInitArguments,
        common: CommonCommandConfiguration,
    },
    Extract {
        project_name: ProjectName,
        common: CommonCommandConfiguration,
    },
    Translate(Box<ConfiguredTranslateCommand>),
    WriteBack(ConfiguredGenericWriteBackCommand),
    Manual(ConfiguredManualCommand),
    Translation(ConfiguredManualCommand),
    Lua(ConfiguredProjectLuaCommand),
}

/// Manual export 读取语言模块筛选待译项；check/apply 只校验结构。
pub(crate) struct ConfiguredManualCommand {
    operation: ManualOperation,
    project_name: ProjectName,
    file: PathBuf,
    export_selection: Option<ManualExportSelection>,
    common: CommonCommandConfiguration,
    language_modules: Option<LanguageModuleCatalog>,
}

impl ConfiguredManualCommand {
    fn build(
        configuration_path: &Path,
        source: &str,
        toml_index: &ConfigurationTomlIndex,
        command: ManualCommand,
        common: CommonCommandConfiguration,
    ) -> Result<Self, ConfigurationLoadError> {
        let language_modules = if matches!(&command, ManualCommand::Export(_)) {
            let raw: RawManualSelection = parse_selected(
                source,
                configuration_path,
                toml_index,
                ConfigurationSelection::Languages,
            )?;
            Some(
                build_language_modules(raw.languages)
                    .map_err(ConfigurationLoadError::InvalidValue)?,
            )
        } else {
            let _: RawInitSelection = parse_selected(
                source,
                configuration_path,
                toml_index,
                ConfigurationSelection::NoAdditionalFields,
            )?;
            None
        };
        Ok(Self::new(command, common, language_modules))
    }

    fn build_rpg_maker(
        configuration_path: &Path,
        source: &str,
        toml_index: &ConfigurationTomlIndex,
        command: RpgMakerManualCommand,
        common: CommonCommandConfiguration,
    ) -> Result<Self, ConfigurationLoadError> {
        let language_modules = if matches!(&command, RpgMakerManualCommand::Export(_)) {
            let raw: RawManualSelection = parse_selected(
                source,
                configuration_path,
                toml_index,
                ConfigurationSelection::Languages,
            )?;
            Some(
                build_language_modules(raw.languages)
                    .map_err(ConfigurationLoadError::InvalidValue)?,
            )
        } else {
            let _: RawInitSelection = parse_selected(
                source,
                configuration_path,
                toml_index,
                ConfigurationSelection::NoAdditionalFields,
            )?;
            None
        };
        let (operation, arguments, export_selection) = match command {
            RpgMakerManualCommand::Export(arguments) => {
                let (manual, selection) = manual_export_parts(arguments);
                (ManualOperation::Export, manual, Some(selection))
            }
            RpgMakerManualCommand::Check(arguments) => (ManualOperation::Check, arguments, None),
            RpgMakerManualCommand::Apply(arguments) => (ManualOperation::Apply, arguments, None),
        };
        Ok(Self::from_parts(
            operation,
            arguments,
            export_selection,
            common,
            language_modules,
        ))
    }

    fn new(
        command: ManualCommand,
        common: CommonCommandConfiguration,
        language_modules: Option<LanguageModuleCatalog>,
    ) -> Self {
        let (operation, arguments, export_selection) = match command {
            ManualCommand::Export(arguments) => {
                let (manual, selection) = manual_export_parts(arguments);
                (ManualOperation::Export, manual, Some(selection))
            }
            ManualCommand::Check(arguments) => (ManualOperation::Check, arguments, None),
            ManualCommand::Apply(arguments) => (ManualOperation::Apply, arguments, None),
        };
        Self::from_parts(
            operation,
            arguments,
            export_selection,
            common,
            language_modules,
        )
    }

    fn build_data_export(
        configuration_path: &Path,
        source: &str,
        toml_index: &ConfigurationTomlIndex,
        command: DataExportCommand,
        operation: ManualOperation,
        common: CommonCommandConfiguration,
        needs_languages: bool,
    ) -> Result<Self, ConfigurationLoadError> {
        let language_modules = if needs_languages {
            let raw: RawManualSelection = parse_selected(
                source,
                configuration_path,
                toml_index,
                ConfigurationSelection::Languages,
            )?;
            Some(
                build_language_modules(raw.languages)
                    .map_err(ConfigurationLoadError::InvalidValue)?,
            )
        } else {
            let _: RawInitSelection = parse_selected(
                source,
                configuration_path,
                toml_index,
                ConfigurationSelection::NoAdditionalFields,
            )?;
            None
        };
        let DataExportCommand::Export(DataExportArguments { project, jsonl }) = command;
        Ok(Self::from_parts(
            operation,
            ManualArguments {
                project,
                file: jsonl,
            },
            None,
            common,
            language_modules,
        ))
    }

    fn from_parts(
        operation: ManualOperation,
        arguments: ManualArguments,
        export_selection: Option<ManualExportSelection>,
        common: CommonCommandConfiguration,
        language_modules: Option<LanguageModuleCatalog>,
    ) -> Self {
        let ManualArguments { project, file } = arguments;
        Self {
            operation,
            project_name: project.name,
            file,
            export_selection,
            common,
            language_modules,
        }
    }

    pub(crate) const fn operation(&self) -> ManualOperation {
        self.operation
    }

    pub(crate) const fn project_name(&self) -> &ProjectName {
        &self.project_name
    }

    pub(crate) fn file(&self) -> &Path {
        &self.file
    }

    pub(crate) const fn export_selection(&self) -> Option<&ManualExportSelection> {
        self.export_selection.as_ref()
    }

    pub(crate) const fn common(&self) -> &CommonCommandConfiguration {
        &self.common
    }

    pub(crate) const fn language_modules(&self) -> Option<&LanguageModuleCatalog> {
        self.language_modules.as_ref()
    }
}

fn manual_export_parts(
    arguments: ManualExportArguments,
) -> (ManualArguments, ManualExportSelection) {
    let ManualExportArguments {
        manual,
        selection,
        ids,
    } = arguments;
    let selection = if let Some(ids) = ids {
        ManualExportSelection::Ids(ids)
    } else {
        match selection.unwrap_or(ManualSelectionArgument::Pending) {
            ManualSelectionArgument::Pending => ManualExportSelection::Pending,
            ManualSelectionArgument::Rejected => ManualExportSelection::Rejected,
            ManualSelectionArgument::All => ManualExportSelection::All,
        }
    };
    (manual, selection)
}

impl ConfiguredGenericCommand {
    fn build(
        configuration_path: &Path,
        distribution: &DistributionLayout,
        source: &str,
        toml_index: Arc<ConfigurationTomlIndex>,
        command: GenericCommand,
    ) -> Result<Self, ConfigurationLoadError> {
        let common = CommonCommandConfiguration::build(distribution.projects_root());

        match command {
            GenericCommand::Init(arguments) => {
                let _: RawInitSelection = parse_selected(
                    source,
                    configuration_path,
                    toml_index.as_ref(),
                    ConfigurationSelection::NoAdditionalFields,
                )?;
                Ok(Self::Init { arguments, common })
            }
            GenericCommand::Extract(project) => {
                let _: RawExtractSelection = parse_selected(
                    source,
                    configuration_path,
                    toml_index.as_ref(),
                    ConfigurationSelection::NoAdditionalFields,
                )?;
                Ok(Self::Extract {
                    project_name: project.name,
                    common,
                })
            }
            GenericCommand::Translate(arguments) => {
                let deferred_source = Arc::new(DeferredConfigurationSource::new(
                    configuration_path,
                    source,
                    Arc::clone(&toml_index),
                ));
                let raw: RawTranslateSelection = parse_selected(
                    source,
                    configuration_path,
                    toml_index.as_ref(),
                    ConfigurationSelection::Translate,
                )?;
                let record_translation_tasks = raw.translation.record_translation_tasks;
                let TranslateArguments {
                    project,
                    profile_id,
                    terms,
                    placeholders,
                    retry_rejected,
                } = arguments;
                let translation = PendingTranslateConfiguration::build(
                    distribution.prompts_root(),
                    raw.prompts,
                    raw.languages,
                    raw.translation,
                )
                .map_err(ConfigurationLoadError::InvalidValue)?;
                let configured = ConfiguredTranslateCommand {
                    project_name: project.name,
                    configuration_path: configuration_path.to_path_buf(),
                    terminology_path: terms,
                    placeholder_rules_path: placeholders,
                    common,
                    cpu: build_cpu_configuration(),
                    record_translation_tasks,
                    retry_rejected,
                    profile: ConfiguredTranslateProfile::Deferred {
                        source: deferred_source,
                        configuration: translation,
                    },
                };
                let configured = match profile_id {
                    Some(profile_id) => configured.resolve_profile(&profile_id)?,
                    None => configured,
                };
                Ok(Self::Translate(Box::new(configured)))
            }
            GenericCommand::WriteBack(arguments) => {
                let raw: RawWriteBackSelection = parse_selected(
                    source,
                    configuration_path,
                    toml_index.as_ref(),
                    ConfigurationSelection::WriteBack,
                )?;
                let publisher =
                    build_directory_publisher_configuration(common.projects_root(), "generic");
                let WriteBackArguments {
                    project,
                    layout_rules,
                } = arguments;
                Ok(Self::WriteBack(ConfiguredGenericWriteBackCommand {
                    project_name: project.name,
                    layout_rules_path: layout_rules,
                    write_back: WriteBackTextConfiguration::from_raw(raw.write_back),
                    common,
                    cpu: build_cpu_configuration(),
                    publisher,
                }))
            }
            GenericCommand::Manual { command } => Ok(Self::Manual(ConfiguredManualCommand::build(
                configuration_path,
                source,
                toml_index.as_ref(),
                command,
                common,
            )?)),
            GenericCommand::Translation { command } => Ok(Self::Translation(
                ConfiguredManualCommand::build_data_export(
                    configuration_path,
                    source,
                    toml_index.as_ref(),
                    command,
                    ManualOperation::TranslationExport,
                    common,
                    false,
                )?,
            )),
            GenericCommand::Lua(arguments) => {
                let raw: RawManualSelection = parse_selected(
                    source,
                    configuration_path,
                    toml_index.as_ref(),
                    ConfigurationSelection::Languages,
                )?;
                let language_modules = build_language_modules(raw.languages)
                    .map_err(ConfigurationLoadError::InvalidValue)?;
                let ProjectLuaArguments {
                    project,
                    script,
                    arguments,
                } = arguments;
                Ok(Self::Lua(ConfiguredProjectLuaCommand {
                    project_name: project.name,
                    common,
                    script: ConfiguredProjectLuaScript::new(script),
                    arguments,
                    language_modules,
                }))
            }
        }
    }
}

/// Generic WriteBack 只解析正文处理和发布所需配置，不读取模型、Prompt 或语言模块。
pub(crate) struct ConfiguredGenericWriteBackCommand {
    project_name: ProjectName,
    layout_rules_path: Option<PathBuf>,
    write_back: WriteBackTextConfiguration,
    common: CommonCommandConfiguration,
    cpu: CpuExecutorConfig,
    publisher: DirectoryPublisherConfig,
}

impl ConfiguredGenericWriteBackCommand {
    pub(crate) const fn common(&self) -> &CommonCommandConfiguration {
        &self.common
    }

    pub(crate) const fn project_name(&self) -> &ProjectName {
        &self.project_name
    }

    pub(crate) fn layout_rules_path(&self) -> Option<&Path> {
        self.layout_rules_path.as_deref()
    }

    pub(crate) const fn write_back(&self) -> WriteBackTextConfiguration {
        self.write_back
    }

    pub(crate) const fn cpu(&self) -> CpuExecutorConfig {
        self.cpu
    }

    pub(crate) const fn publisher(&self) -> &DirectoryPublisherConfig {
        &self.publisher
    }
}

pub(crate) struct CommonCommandConfiguration {
    projects_root: PathBuf,
    filesystem: SystemFileSystemConfig,
    sqlite: RusqliteStorageConfiguration,
}

impl CommonCommandConfiguration {
    fn build(projects_root: &Path) -> Self {
        Self {
            projects_root: projects_root.to_path_buf(),
            filesystem: build_file_system_configuration(),
            sqlite: build_sqlite_configuration(),
        }
    }

    pub(crate) fn projects_root(&self) -> &Path {
        &self.projects_root
    }

    pub(crate) const fn filesystem(&self) -> &SystemFileSystemConfig {
        &self.filesystem
    }

    pub(crate) const fn sqlite(&self) -> &RusqliteStorageConfiguration {
        &self.sqlite
    }

    #[cfg(test)]
    pub(crate) fn for_test(projects_root: &Path) -> Self {
        Self::build(projects_root)
    }
}

pub(crate) struct ConfiguredInitCommand {
    pub(crate) arguments: InitArguments,
    common: CommonCommandConfiguration,
    publisher: DirectoryPublisherConfig,
}

impl ConfiguredInitCommand {
    pub(crate) const fn common(&self) -> &CommonCommandConfiguration {
        &self.common
    }

    pub(crate) const fn publisher(&self) -> &DirectoryPublisherConfig {
        &self.publisher
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        arguments: InitArguments,
        projects_root: &Path,
        engine_storage_name: &str,
    ) -> Self {
        Self {
            arguments,
            common: CommonCommandConfiguration::build(projects_root),
            publisher: build_directory_publisher_configuration(projects_root, engine_storage_name),
        }
    }
}

pub(crate) struct ConfiguredExtractCommand {
    project_name: ProjectName,
    common: CommonCommandConfiguration,
    cpu: CpuExecutorConfig,
    rpg_maker: ExtractConfiguration,
    dialogue_rules_path: Option<PathBuf>,
}

impl ConfiguredExtractCommand {
    pub(crate) const fn common(&self) -> &CommonCommandConfiguration {
        &self.common
    }

    pub(crate) const fn project_name(&self) -> &ProjectName {
        &self.project_name
    }

    pub(crate) const fn cpu(&self) -> CpuExecutorConfig {
        self.cpu
    }

    pub(crate) const fn rpg_maker(&self) -> &ExtractConfiguration {
        &self.rpg_maker
    }

    pub(crate) fn dialogue_rules_path(&self) -> Option<&Path> {
        self.dialogue_rules_path.as_deref()
    }
}

pub(crate) struct ConfiguredTranslateCommand {
    project_name: ProjectName,
    configuration_path: PathBuf,
    terminology_path: Option<PathBuf>,
    placeholder_rules_path: Option<PathBuf>,
    common: CommonCommandConfiguration,
    cpu: CpuExecutorConfig,
    record_translation_tasks: bool,
    retry_rejected: bool,
    profile: ConfiguredTranslateProfile,
}

enum ConfiguredTranslateProfile {
    Deferred {
        source: Arc<DeferredConfigurationSource>,
        configuration: PendingTranslateConfiguration,
    },
    Resolved(Box<TranslateConfiguration>),
}

impl ConfiguredTranslateCommand {
    pub(crate) const fn common(&self) -> &CommonCommandConfiguration {
        &self.common
    }

    pub(crate) const fn project_name(&self) -> &ProjectName {
        &self.project_name
    }

    pub(crate) fn terminology_path(&self) -> Option<&Path> {
        self.terminology_path.as_deref()
    }

    pub(crate) fn placeholder_rules_path(&self) -> Option<&Path> {
        self.placeholder_rules_path.as_deref()
    }

    pub(crate) const fn cpu(&self) -> CpuExecutorConfig {
        self.cpu
    }

    pub(crate) fn llm(&self) -> &SelectedLlmExecutorConfiguration {
        self.translation().llm()
    }

    pub(crate) const fn record_translation_tasks(&self) -> bool {
        self.record_translation_tasks
    }

    pub(crate) const fn retry_rejected(&self) -> bool {
        self.retry_rejected
    }

    /// 返回已经在命令行显式选择并于加载阶段完成校验的 Profile。
    ///
    /// `None` 表示调用方必须从项目运行方案取得 Profile，并调用
    /// [`Self::resolve_profile`]；不会隐式选择配置中的其他条目。
    pub(crate) fn resolved_profile_id(&self) -> Option<&str> {
        match &self.profile {
            ConfiguredTranslateProfile::Deferred { .. } => None,
            ConfiguredTranslateProfile::Resolved(configuration) => {
                Some(configuration.profile().id())
            }
        }
    }

    /// 消费待解析命令，并精确选择调用方提供的 Profile。
    ///
    /// 显式 Profile 已在加载阶段解析；用同一 ID 再调用是幂等的。若调用方
    /// 提供不同 ID，则返回配置错误，避免项目状态覆盖显式命令行意图。
    pub(crate) fn resolve_profile(self, profile_id: &str) -> Result<Self, ConfigurationLoadError> {
        let configuration_path = self.configuration_path().to_path_buf();
        validate_exact_identifier("Profile ID", profile_id)
            .map_err(ConfigurationLoadError::InvalidValue)
            .map_err(|error| error.with_configuration_path(&configuration_path))?;

        let Self {
            project_name,
            configuration_path,
            terminology_path,
            placeholder_rules_path,
            common,
            cpu,
            record_translation_tasks,
            retry_rejected,
            profile,
        } = self;
        let profile = match profile {
            ConfiguredTranslateProfile::Deferred {
                source,
                configuration,
            } => ConfiguredTranslateProfile::Resolved(Box::new(
                configuration.resolve(source.as_ref(), profile_id)?,
            )),
            ConfiguredTranslateProfile::Resolved(configuration)
                if configuration.profile().id() == profile_id =>
            {
                ConfiguredTranslateProfile::Resolved(configuration)
            }
            ConfiguredTranslateProfile::Resolved(configuration) => {
                let explicit_profile = configuration.profile().id().to_owned();
                return Err(ConfigurationLoadError::ProfileSelectionConflict {
                    path: configuration_path,
                    explicit_profile,
                    requested_profile: profile_id.to_owned(),
                });
            }
        };
        Ok(Self {
            project_name,
            configuration_path,
            terminology_path,
            placeholder_rules_path,
            common,
            cpu,
            record_translation_tasks,
            retry_rejected,
            profile,
        })
    }

    fn configuration_path(&self) -> &Path {
        &self.configuration_path
    }

    #[cfg(test)]
    pub(crate) fn client(&self) -> &Arc<OpenAiCompatibleClient> {
        self.translation().client()
    }

    pub(crate) fn translation(&self) -> &TranslateConfiguration {
        match &self.profile {
            ConfiguredTranslateProfile::Deferred { .. } => {
                panic!("Translate Profile 必须在业务装配前完成解析")
            }
            ConfiguredTranslateProfile::Resolved(configuration) => configuration,
        }
    }
}

/// 一次性项目 Lua 命令已经建立的进程级配置。
pub(crate) struct ConfiguredProjectLuaCommand {
    project_name: ProjectName,
    common: CommonCommandConfiguration,
    script: ConfiguredProjectLuaScript,
    arguments: Vec<String>,
    language_modules: LanguageModuleCatalog,
}

impl ConfiguredProjectLuaCommand {
    pub(crate) const fn common(&self) -> &CommonCommandConfiguration {
        &self.common
    }

    pub(crate) const fn project_name(&self) -> &ProjectName {
        &self.project_name
    }

    pub(crate) const fn script(&self) -> &ConfiguredProjectLuaScript {
        &self.script
    }

    pub(crate) fn arguments(&self) -> &[String] {
        &self.arguments
    }

    pub(crate) const fn language_modules(&self) -> &LanguageModuleCatalog {
        &self.language_modules
    }
}

pub(crate) struct ConfiguredWriteBackCommand {
    project_name: ProjectName,
    layout_rules_path: Option<PathBuf>,
    common: CommonCommandConfiguration,
    cpu: CpuExecutorConfig,
    publisher: DirectoryPublisherConfig,
    rpg_maker: WriteBackConfiguration,
}

impl ConfiguredWriteBackCommand {
    pub(crate) const fn common(&self) -> &CommonCommandConfiguration {
        &self.common
    }

    pub(crate) const fn project_name(&self) -> &ProjectName {
        &self.project_name
    }

    pub(crate) fn layout_rules_path(&self) -> Option<&Path> {
        self.layout_rules_path.as_deref()
    }

    pub(crate) const fn cpu(&self) -> CpuExecutorConfig {
        self.cpu
    }

    pub(crate) const fn publisher(&self) -> &DirectoryPublisherConfig {
        &self.publisher
    }

    pub(crate) const fn rpg_maker(&self) -> &WriteBackConfiguration {
        &self.rpg_maker
    }
}

fn build_cpu_configuration() -> CpuExecutorConfig {
    CpuExecutorConfig::production()
}

fn build_file_system_configuration() -> SystemFileSystemConfig {
    SystemFileSystemConfig::production()
}

fn build_directory_publisher_configuration(
    projects_root: &Path,
    engine_storage_name: &str,
) -> DirectoryPublisherConfig {
    DirectoryPublisherConfig::production(
        projects_root
            .join(".att-locks")
            .join("directory-publish")
            .join(engine_storage_name),
    )
    .expect("固定项目目录派生的发布锁路径不得为空")
}

fn build_sqlite_configuration() -> RusqliteStorageConfiguration {
    RusqliteStorageConfiguration::production()
}

/// 保留仅能在项目运行方案解析后按需消费的配置原文。
///
/// 配置可能包含凭据，因此不实现 `Debug`，并在最后一个引用释放时清零正文。
pub(crate) struct DeferredConfigurationSource {
    path: PathBuf,
    source: Zeroizing<String>,
    toml_index: Arc<ConfigurationTomlIndex>,
}

impl DeferredConfigurationSource {
    fn new(path: &Path, source: &str, toml_index: Arc<ConfigurationTomlIndex>) -> Self {
        Self {
            path: path.to_path_buf(),
            source: Zeroizing::new(source.to_owned()),
            toml_index,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn source(&self) -> &str {
        self.source.as_str()
    }

    fn toml_index(&self) -> &ConfigurationTomlIndex {
        self.toml_index.as_ref()
    }
}

pub(crate) struct ConfiguredProjectLuaScript {
    script_path: PathBuf,
}

impl ConfiguredProjectLuaScript {
    fn new(script_path: PathBuf) -> Self {
        Self { script_path }
    }

    pub(crate) fn script_path(&self) -> &Path {
        &self.script_path
    }
}

#[derive(Clone)]
pub(crate) struct SelectedLlmExecutorConfiguration {
    runtime: OpenAiExecutorConfiguration,
    additional_pem_files: Vec<PathBuf>,
}

impl SelectedLlmExecutorConfiguration {
    pub(crate) fn additional_pem_files(&self) -> &[PathBuf] {
        &self.additional_pem_files
    }

    pub(crate) fn with_pem_roots(&self, roots: Vec<Vec<u8>>) -> OpenAiExecutorConfiguration {
        self.runtime.clone().with_additional_pem_roots(roots)
    }
}

pub(crate) struct SelectedRulesConfiguration {
    rules_path: PathBuf,
}

impl SelectedRulesConfiguration {
    pub(crate) fn rules_path(&self) -> &Path {
        &self.rules_path
    }
}

pub(crate) struct ExtractConfiguration {
    document: RpgMakerDocumentReadingConfig,
    builtin: bool,
    rules: Option<SelectedRulesConfiguration>,
}

impl ExtractConfiguration {
    fn build(select_builtin: bool, rules_path: Option<PathBuf>) -> Self {
        let rules = rules_path.map(|rules_path| SelectedRulesConfiguration { rules_path });
        Self {
            document: build_document_configuration(),
            builtin: select_builtin,
            rules,
        }
    }

    pub(crate) const fn document(&self) -> RpgMakerDocumentReadingConfig {
        self.document
    }

    pub(crate) const fn builtin(&self) -> bool {
        self.builtin
    }

    pub(crate) const fn rules(&self) -> Option<&SelectedRulesConfiguration> {
        self.rules.as_ref()
    }
}

pub(crate) struct TranslateConfiguration {
    prompt_root: PathBuf,
    thinking_output: bool,
    source_echo: bool,
    language_modules: LanguageModuleCatalog,
    profile: TranslationProfileConfiguration,
    client: Arc<OpenAiCompatibleClient>,
    llm: SelectedLlmExecutorConfiguration,
}

struct PendingTranslateConfiguration {
    prompt_root: PathBuf,
    thinking_output: bool,
    source_echo: bool,
    language_modules: LanguageModuleCatalog,
}

impl PendingTranslateConfiguration {
    fn build(
        prompt_root: &Path,
        raw_prompts: RawPromptsConfiguration,
        raw_languages: Vec<RawLanguageConfiguration>,
        _raw: RawTranslationSelection,
    ) -> Result<Self, ConfigurationValueError> {
        Ok(Self {
            prompt_root: prompt_root.to_path_buf(),
            thinking_output: raw_prompts.thinking_output,
            source_echo: raw_prompts.source_echo,
            language_modules: build_language_modules(raw_languages)?,
        })
    }

    fn resolve(
        self,
        source: &DeferredConfigurationSource,
        profile_id: &str,
    ) -> Result<TranslateConfiguration, ConfigurationLoadError> {
        let selected_profile = parse_selected_translation_profile(
            source.source(),
            source.path(),
            source.toml_index(),
            profile_id,
        )?;
        let llm_client_id = selected_profile.llm_client.clone();
        let raw_client = parse_selected_llm_client(
            source.source(),
            source.path(),
            source.toml_index(),
            &llm_client_id,
        )?;
        let built_client = build_llm_client(
            format!("llm.clients.{llm_client_id}").as_str(),
            source.path().parent().expect("配置文件必须拥有父目录"),
            raw_client,
        )
        .map_err(ConfigurationLoadError::InvalidValue)
        .map_err(|error| error.with_configuration_path(source.path()))?;
        let profile = build_selected_translation_profile(
            "translation.profiles",
            selected_profile,
            built_client.request,
        )
        .map_err(ConfigurationLoadError::InvalidValue)
        .map_err(|error| error.with_configuration_path(source.path()))?;
        Ok(TranslateConfiguration {
            prompt_root: self.prompt_root,
            thinking_output: self.thinking_output,
            source_echo: self.source_echo,
            language_modules: self.language_modules,
            profile,
            client: Arc::new(built_client.client),
            llm: built_client.executor,
        })
    }
}

impl TranslateConfiguration {
    pub(crate) const fn profile(&self) -> &TranslationProfileConfiguration {
        &self.profile
    }

    pub(crate) fn prompt_root(&self) -> &Path {
        &self.prompt_root
    }

    pub(crate) const fn thinking_output(&self) -> bool {
        self.thinking_output
    }

    pub(crate) const fn source_echo(&self) -> bool {
        self.source_echo
    }

    pub(crate) const fn language_modules(&self) -> &LanguageModuleCatalog {
        &self.language_modules
    }

    pub(crate) const fn client(&self) -> &Arc<OpenAiCompatibleClient> {
        &self.client
    }

    pub(crate) const fn llm(&self) -> &SelectedLlmExecutorConfiguration {
        &self.llm
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WriteBackTextConfiguration {
    repair_punctuation: bool,
    complete_continuation_whitespace: bool,
}

impl WriteBackTextConfiguration {
    const fn from_raw(raw: RawWriteBackConfiguration) -> Self {
        Self {
            repair_punctuation: raw.repair_punctuation,
            complete_continuation_whitespace: raw.complete_continuation_whitespace,
        }
    }

    pub(crate) const fn repair_punctuation(self) -> bool {
        self.repair_punctuation
    }

    pub(crate) const fn complete_continuation_whitespace(self) -> bool {
        self.complete_continuation_whitespace
    }
}

pub(crate) struct WriteBackConfiguration {
    document: RpgMakerDocumentReadingConfig,
    text: WriteBackTextConfiguration,
}

impl WriteBackConfiguration {
    fn build(raw: RawWriteBackConfiguration) -> Self {
        Self {
            document: build_document_configuration(),
            text: WriteBackTextConfiguration::from_raw(raw),
        }
    }

    pub(crate) const fn document(&self) -> RpgMakerDocumentReadingConfig {
        self.document
    }

    pub(crate) const fn text(&self) -> WriteBackTextConfiguration {
        self.text
    }
}

fn build_document_configuration() -> RpgMakerDocumentReadingConfig {
    RpgMakerDocumentReadingConfig::new(NonZeroUsize::new(8).expect("产品并发值必须非零"))
}

#[derive(Clone, Debug)]
pub(crate) struct TranslationProfileConfiguration {
    id: String,
    /// 完整原文稳定投影的 TaskBlock 装箱目标，不是最终 user message 硬上限。
    target_task_user_message_characters: NonZeroUsize,
    request: TranslationRequestConfiguration,
}

impl TranslationProfileConfiguration {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) const fn target_task_user_message_characters(&self) -> NonZeroUsize {
        self.target_task_user_message_characters
    }

    pub(crate) const fn request(&self) -> &TranslationRequestConfiguration {
        &self.request
    }
}

fn build_language_modules(
    raw_languages: Vec<RawLanguageConfiguration>,
) -> Result<LanguageModuleCatalog, ConfigurationValueError> {
    let mut bindings = Vec::<(LanguageId, Arc<dyn LanguageModule>)>::new();
    let mut language_ids = BTreeSet::new();
    for (index, raw) in raw_languages.into_iter().enumerate() {
        let field = format!("languages[{index}]");
        let (id, module): (String, Arc<dyn LanguageModule>) = match raw {
            RawLanguageConfiguration::Japanese {
                id,
                minimum_kana_characters,
                allowed_terms,
            } => {
                let residual = JapaneseResidualPolicy::new(
                    non_zero_usize(
                        format!("{field}.minimum_kana_characters").as_str(),
                        minimum_kana_characters,
                    )?,
                    allowed_terms,
                )
                .map_err(|source| invalid(field.as_str(), language_policy_rule(&source)))?;
                (id, Arc::new(JapaneseLanguageModule::new(residual)))
            }
            RawLanguageConfiguration::English {
                id,
                minimum_word_count,
                minimum_letter_count,
                ignored_terms,
                minimum_copied_word_count,
                minimum_copied_letter_count,
                allowed_terms,
            } => {
                let detection = EnglishTranslationDetectionPolicy::new(
                    non_zero_usize(
                        format!("{field}.minimum_word_count").as_str(),
                        minimum_word_count,
                    )?,
                    non_zero_usize(
                        format!("{field}.minimum_letter_count").as_str(),
                        minimum_letter_count,
                    )?,
                    ignored_terms,
                )
                .map_err(|source| invalid(field.as_str(), language_policy_rule(&source)))?;
                let residual = EnglishResidualPolicy::new(
                    non_zero_usize(
                        format!("{field}.minimum_copied_word_count").as_str(),
                        minimum_copied_word_count,
                    )?,
                    non_zero_usize(
                        format!("{field}.minimum_copied_letter_count").as_str(),
                        minimum_copied_letter_count,
                    )?,
                    allowed_terms,
                )
                .map_err(|source| invalid(field.as_str(), language_policy_rule(&source)))?;
                (
                    id,
                    Arc::new(EnglishLanguageModule::new(detection, residual)),
                )
            }
        };
        let id = LanguageId::parse(&id)
            .map_err(|source| invalid(format!("{field}.id").as_str(), language_id_rule(&source)))?;
        if !language_ids.insert(id.as_str().to_owned()) {
            return Err(invalid(
                field.as_str(),
                ConfigurationValueRule::LanguageIdDuplicate,
            ));
        }
        bindings.push((id, module));
    }

    let catalog = LanguageModuleCatalog::new(bindings)
        .map_err(|source| invalid("languages", language_catalog_rule(&source)))?;
    Ok(catalog)
}

const fn language_policy_rule(source: &LanguagePolicyConfigurationError) -> ConfigurationValueRule {
    match source {
        LanguagePolicyConfigurationError::BlankTerm => {
            ConfigurationValueRule::LanguagePolicyTermBlank
        }
        LanguagePolicyConfigurationError::SurroundingWhitespace { .. } => {
            ConfigurationValueRule::LanguagePolicyTermSurroundingWhitespace
        }
        LanguagePolicyConfigurationError::DuplicateTerm { .. } => {
            ConfigurationValueRule::LanguagePolicyTermDuplicate
        }
    }
}

const fn language_id_rule(source: &LanguageIdError) -> ConfigurationValueRule {
    match source {
        LanguageIdError::Blank => ConfigurationValueRule::LanguageIdBlank,
        LanguageIdError::SurroundingWhitespace { .. } => {
            ConfigurationValueRule::LanguageIdSurroundingWhitespace
        }
        LanguageIdError::Underscore { .. } => ConfigurationValueRule::LanguageIdUsesUnderscore,
        LanguageIdError::InvalidSyntax { .. } => ConfigurationValueRule::LanguageIdInvalidSyntax,
        LanguageIdError::InvalidRegistryTag { .. } => {
            ConfigurationValueRule::LanguageIdInvalidRegistryTag
        }
        LanguageIdError::CanonicalizationFailed { .. } => {
            ConfigurationValueRule::LanguageIdCanonicalizationFailed
        }
        LanguageIdError::UndefinedPrimaryLanguage { .. } => {
            ConfigurationValueRule::LanguageIdUndefinedPrimaryLanguage
        }
    }
}

const fn language_catalog_rule(source: &LanguageModuleCatalogBuildError) -> ConfigurationValueRule {
    match source {
        LanguageModuleCatalogBuildError::MissingLanguageModule => {
            ConfigurationValueRule::LanguageCatalogEmpty
        }
        LanguageModuleCatalogBuildError::DuplicateLanguageId { .. } => {
            ConfigurationValueRule::LanguageIdDuplicate
        }
    }
}

fn build_selected_translation_profile(
    field: &str,
    raw: RawSelectedTranslationProfileConfiguration,
    request: TranslationRequestConfiguration,
) -> Result<TranslationProfileConfiguration, ConfigurationValueError> {
    validate_exact_identifier(format!("{field}.id").as_str(), &raw.id)?;
    validate_exact_identifier(format!("{field}.llm_client").as_str(), &raw.llm_client)?;
    Ok(TranslationProfileConfiguration {
        id: raw.id,
        target_task_user_message_characters: non_zero_usize(
            format!("{field}.target_task_user_message_characters").as_str(),
            raw.target_task_user_message_characters,
        )?,
        request,
    })
}

struct BuiltLlmClient {
    executor: SelectedLlmExecutorConfiguration,
    client: OpenAiCompatibleClient,
    request: TranslationRequestConfiguration,
}

fn build_llm_client(
    field: &str,
    configuration_directory: &Path,
    raw: RawLlmClientConfiguration,
) -> Result<BuiltLlmClient, ConfigurationValueError> {
    let protocol = OpenAiProtocol::from(raw.protocol);
    let url = Url::parse(&raw.url).map_err(|_| {
        invalid(
            format!("{field}.url").as_str(),
            ConfigurationValueRule::UrlInvalid,
        )
    })?;
    validate_llm_url(format!("{field}.url").as_str(), &url)?;
    validate_exact_identifier(format!("{field}.model").as_str(), &raw.model)?;

    let exposed_api_key = raw.api_key.expose_secret();
    if exposed_api_key.trim().is_empty() {
        return Err(invalid(
            format!("{field}.api_key").as_str(),
            ConfigurationValueRule::ApiKeyBlank,
        ));
    }
    if exposed_api_key.trim() != exposed_api_key {
        return Err(invalid(
            format!("{field}.api_key").as_str(),
            ConfigurationValueRule::ApiKeySurroundingWhitespace,
        ));
    }
    if reqwest::header::HeaderValue::from_bytes(exposed_api_key.as_bytes()).is_err() {
        return Err(invalid(
            format!("{field}.api_key").as_str(),
            ConfigurationValueRule::ApiKeyInvalidHeader,
        ));
    }

    // 任意精度数字会在 Serde 访问器内使用一个内部 map 信封传递原始十进制
    // 文本。第一遍自定义访问器只负责递归拒绝重复键；第二遍由
    // `serde_json::Value` 自身还原真正的 Number，避免把内部信封写入请求正文。
    serde_json::from_str::<StrictJsonValue>(&raw.parameters).map_err(|error| {
        invalid(
            format!("{field}.parameters").as_str(),
            ConfigurationValueRule::StrictJsonInvalid {
                line: u64::try_from(error.line())
                    .expect("当前目标平台的 JSON 行号必须能用 u64 表达"),
                column: u64::try_from(error.column())
                    .expect("当前目标平台的 JSON 列号必须能用 u64 表达"),
            },
        )
    })?;
    let parameter_value = serde_json::from_str::<JsonValue>(&raw.parameters)
        .expect("已通过同一 serde_json 语法边界的源文必须可重建为 Value");
    let JsonValue::Object(parameters) = parameter_value else {
        return Err(invalid(
            format!("{field}.parameters").as_str(),
            ConfigurationValueRule::JsonObjectRequired,
        ));
    };
    let reserved_fields: &[&str] = match protocol {
        OpenAiProtocol::ChatCompletions => &CHAT_COMPLETIONS_RESERVED_REQUEST_BODY_FIELDS,
        OpenAiProtocol::Responses => &RESPONSES_RESERVED_REQUEST_BODY_FIELDS,
    };
    for &reserved in reserved_fields {
        if parameters.contains_key(reserved) {
            return Err(invalid(
                format!("{field}.parameters.{reserved}").as_str(),
                ConfigurationValueRule::ReservedRequestField,
            ));
        }
    }

    let proxy = match raw.proxy {
        RawProxyConfiguration::Disabled(false) => LlmProxyConfiguration::Disabled,
        RawProxyConfiguration::Disabled(true) => {
            return Err(invalid(
                format!("{field}.proxy").as_str(),
                ConfigurationValueRule::ProxyMustBeFalseOrUrl,
            ));
        }
        RawProxyConfiguration::Url(value) => {
            let url = Url::parse(&value).map_err(|_| {
                invalid(
                    format!("{field}.proxy").as_str(),
                    ConfigurationValueRule::UrlInvalid,
                )
            })?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(invalid(
                    format!("{field}.proxy").as_str(),
                    ConfigurationValueRule::UrlSchemeUnsupported,
                ));
            }
            if !url.username().is_empty() || url.password().is_some() {
                return Err(invalid(
                    format!("{field}.proxy").as_str(),
                    ConfigurationValueRule::UrlCredentialsForbidden,
                ));
            }
            LlmProxyConfiguration::Explicit(url)
        }
    };
    let mut additional_pem_files = Vec::with_capacity(raw.additional_pem_files.len());
    let mut seen_pem_files = BTreeSet::new();
    for (index, path) in raw.additional_pem_files.into_iter().enumerate() {
        let pem_field = format!("{field}.additional_pem_files[{index}]");
        let path = checked_path(&pem_field, configuration_directory, path)?;
        if !seen_pem_files.insert(path.clone()) {
            return Err(invalid(
                &pem_field,
                ConfigurationValueRule::PemPathDuplicate,
            ));
        }
        additional_pem_files.push(path);
    }
    let max_concurrent_requests = non_zero_usize(
        format!("{field}.max_concurrent_requests").as_str(),
        raw.max_concurrent_requests,
    )?;
    if max_concurrent_requests.get() > tokio::sync::Semaphore::MAX_PERMITS {
        return Err(invalid(
            format!("{field}.max_concurrent_requests").as_str(),
            ConfigurationValueRule::RuntimeMaximumExceeded {
                actual: u64::try_from(max_concurrent_requests.get())
                    .expect("当前目标平台的并发请求数必须能用 u64 表达"),
                maximum: u64::try_from(tokio::sync::Semaphore::MAX_PERMITS)
                    .expect("当前目标平台的信号量许可上限必须能用 u64 表达"),
            },
        ));
    }
    let connect_timeout = positive_duration(
        format!("{field}.connect_timeout_ms").as_str(),
        raw.connect_timeout_ms,
    )?;
    let read_timeout = positive_duration(
        format!("{field}.read_timeout_ms").as_str(),
        raw.read_timeout_ms,
    )?;
    let request_timeout = positive_duration(
        format!("{field}.request_timeout_ms").as_str(),
        raw.request_timeout_ms,
    )?;
    let rate_limit = raw
        .rate_limit
        .map(|rate| {
            Ok((
                non_zero_u32(
                    format!("{field}.rate_limit.requests_per_minute").as_str(),
                    rate.requests_per_minute,
                )?,
                non_zero_u32(format!("{field}.rate_limit.burst").as_str(), rate.burst)?,
            ))
        })
        .transpose()?;
    let request = TranslationRequestConfiguration::new(
        raw.retry_delays_ms
            .into_iter()
            .map(Duration::from_millis)
            .collect(),
        Duration::from_millis(raw.max_retry_after_ms),
    );
    let client = OpenAiCompatibleClient::new_with_endpoint(
        OpenAiEndpoint::new(url, protocol, raw.stream),
        raw.api_key,
        raw.model,
        max_concurrent_requests,
        request_timeout,
        rate_limit,
        parameters,
    );
    Ok(BuiltLlmClient {
        executor: SelectedLlmExecutorConfiguration {
            runtime: OpenAiExecutorConfiguration::new(
                max_concurrent_requests,
                connect_timeout,
                read_timeout,
                request.max_network_retry_after(),
                proxy,
            ),
            additional_pem_files,
        },
        client,
        request,
    })
}

fn validate_llm_url(field: &str, url: &Url) -> Result<(), ConfigurationValueError> {
    if url.username() != "" || url.password().is_some() {
        return Err(invalid(
            field,
            ConfigurationValueRule::UrlCredentialsForbidden,
        ));
    }
    if url.fragment().is_some() {
        return Err(invalid(field, ConfigurationValueRule::UrlFragmentForbidden));
    }
    match url.scheme() {
        "http" | "https" => Ok(()),
        _ => Err(invalid(field, ConfigurationValueRule::UrlSchemeUnsupported)),
    }
}

fn validate_exact_identifier(field: &str, value: &str) -> Result<(), ConfigurationValueError> {
    validate_non_blank(field, value)?;
    if value.trim() != value {
        return Err(invalid(
            field,
            ConfigurationValueRule::ValueSurroundingWhitespace,
        ));
    }
    Ok(())
}

fn validate_non_blank(field: &str, value: &str) -> Result<(), ConfigurationValueError> {
    if value.trim().is_empty() {
        Err(invalid(field, ConfigurationValueRule::ValueBlank))
    } else {
        Ok(())
    }
}

fn checked_path(
    field: &str,
    configuration_directory: &Path,
    value: PathBuf,
) -> Result<PathBuf, ConfigurationValueError> {
    if value.as_os_str().is_empty() {
        return Err(invalid(field, ConfigurationValueRule::PathBlank));
    }
    Ok(resolve_path(configuration_directory, &value))
}

fn resolve_path(base: &Path, value: &Path) -> PathBuf {
    if value.is_absolute() {
        value.to_path_buf()
    } else {
        base.join(value)
    }
}

fn source_location(source: &str, byte_offset: usize) -> SourceLocation {
    let prefix = source.get(..byte_offset).unwrap_or(source);
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit_once('\n')
        .map_or(prefix, |(_, current_line)| current_line)
        .chars()
        .count()
        + 1;
    SourceLocation { line, column }
}

struct StrictJsonValue(JsonValue);

impl<'de> Deserialize<'de> for StrictJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictJsonVisitor).map(Self)
    }
}

struct StrictJsonVisitor;

impl<'de> Visitor<'de> for StrictJsonVisitor {
    type Value = JsonValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("任意合法 JSON 值")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(JsonValue::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(JsonValue::Number(value.into()))
    }

    fn visit_i128<E>(self, value: i128) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        JsonNumber::deserialize(serde::de::value::I128Deserializer::new(value))
            .map(JsonValue::Number)
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(JsonValue::Number(value.into()))
    }

    fn visit_u128<E>(self, value: u128) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        JsonNumber::deserialize(serde::de::value::U128Deserializer::new(value))
            .map(JsonValue::Number)
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E> {
        Ok(JsonNumber::from_f64(value).map_or(JsonValue::Null, JsonValue::Number))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(JsonValue::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(JsonValue::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(JsonValue::Null)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        StrictJsonValue::deserialize(deserializer).map(|value| value.0)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(JsonValue::Null)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<StrictJsonValue>()? {
            values.push(value.0);
        }
        Ok(JsonValue::Array(values))
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = JsonMap::new();
        while let Some(key) = object.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(de::Error::custom("JSON 对象包含重复字段"));
            }
            let value = object.next_value::<StrictJsonValue>()?;
            values.insert(key, value.0);
        }
        Ok(JsonValue::Object(values))
    }
}

fn deserialize_api_key<'de, D>(deserializer: D) -> Result<SecretString, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer).map(SecretString::from)
}

fn non_zero_usize(field: &str, value: u64) -> Result<NonZeroUsize, ConfigurationValueError> {
    let value = usize_value(field, value)?;
    NonZeroUsize::new(value).ok_or_else(|| {
        invalid(
            field,
            ConfigurationValueRule::PositiveRequired { actual: 0 },
        )
    })
}

fn usize_value(field: &str, value: u64) -> Result<usize, ConfigurationValueError> {
    usize::try_from(value).map_err(|_| {
        invalid(
            field,
            ConfigurationValueRule::UsizeRangeExceeded { actual: value },
        )
    })
}

fn non_zero_u32(field: &str, value: u64) -> Result<NonZeroU32, ConfigurationValueError> {
    let value = u32::try_from(value).map_err(|_| {
        invalid(
            field,
            ConfigurationValueRule::U32RangeExceeded { actual: value },
        )
    })?;
    NonZeroU32::new(value).ok_or_else(|| {
        invalid(
            field,
            ConfigurationValueRule::PositiveRequired { actual: 0 },
        )
    })
}

fn positive_duration(field: &str, milliseconds: u64) -> Result<Duration, ConfigurationValueError> {
    if milliseconds == 0 {
        return Err(invalid(
            field,
            ConfigurationValueRule::PositiveRequired { actual: 0 },
        ));
    }
    Ok(Duration::from_millis(milliseconds))
}

pub(super) fn invalid(field: &str, rule: ConfigurationValueRule) -> ConfigurationValueError {
    ConfigurationValueError {
        field: field.to_owned(),
        rule,
    }
}

#[derive(Debug)]
pub(crate) enum DistributionLayoutError {
    CurrentExecutable(io::Error),
    ExecutableDirectoryMissing { path: PathBuf },
}

impl DistributionLayoutError {
    pub(crate) fn diagnostic(&self) -> crate::diagnostic::Diagnostic {
        use crate::diagnostic::{
            Diagnostic, FileSystemDiagnosticContext, FileSystemDiagnosticStage, FileSystemIssue,
            FileSystemOperation, FileSystemPathViolation, FileSystemProblem, IoFailure,
            RuntimeComponent, RuntimeIssue, RuntimeOperation, SafePath,
        };

        match self {
            Self::CurrentExecutable(source) => Diagnostic::runtime(RuntimeIssue::Io {
                component: RuntimeComponent::Process,
                operation: RuntimeOperation::ResolveCurrentExecutable,
                failure: IoFailure::from_error(source),
            }),
            Self::ExecutableDirectoryMissing { path } => {
                Diagnostic::file_system(FileSystemIssue::new(
                    FileSystemDiagnosticContext::new(
                        FileSystemDiagnosticStage::ProcessStartup,
                        FileSystemOperation::ResolveDirectory,
                    ),
                    FileSystemProblem::InvalidPath {
                        path: SafePath::new(path),
                        violation: FileSystemPathViolation::MissingParent,
                    },
                ))
            }
        }
    }
}

impl fmt::Display for DistributionLayoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentExecutable(source) => {
                write!(formatter, "无法确定当前可执行文件：{source}")
            }
            Self::ExecutableDirectoryMissing { path } => {
                write!(formatter, "当前可执行文件没有发行目录：{}", path.display())
            }
        }
    }
}

impl Error for DistributionLayoutError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CurrentExecutable(source) => Some(source),
            Self::ExecutableDirectoryMissing { .. } => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum ConfigurationLoadError {
    Open {
        path: PathBuf,
        source: io::Error,
    },
    NotAFile {
        path: PathBuf,
    },
    Read {
        path: PathBuf,
        source: io::Error,
    },
    InvalidUtf8 {
        path: PathBuf,
        valid_up_to: usize,
        error_len: Option<usize>,
    },
    InvalidToml {
        path: PathBuf,
        location: Option<SourceLocation>,
        resource: String,
        failure: ConfigurationTomlFailureKind,
    },
    InvalidValue(ConfigurationValueError),
    InvalidValueAtPath {
        path: PathBuf,
        source: ConfigurationValueError,
    },
    TranslationProfileNotFound {
        path: PathBuf,
        profile_id: String,
    },
    ProfileSelectionConflict {
        path: PathBuf,
        explicit_profile: String,
        requested_profile: String,
    },
}

impl ConfigurationLoadError {
    fn with_configuration_path(self, path: &Path) -> Self {
        match self {
            Self::InvalidValue(source) => Self::InvalidValueAtPath {
                path: path.to_path_buf(),
                source,
            },
            other => other,
        }
    }

    /// 在仍持有路径、字段和解析位置时建立当前版本的封闭诊断。
    pub(crate) fn diagnostic(&self) -> crate::diagnostic::Diagnostic {
        use crate::diagnostic::{
            ConfigurationIssue, Diagnostic, IoFailure, SafeIdentifier, SafePath, SafeText,
        };

        let issue = match self {
            Self::Open { path, source } => ConfigurationIssue::Open {
                path: SafePath::new(path),
                failure: IoFailure::from_error(source),
            },
            Self::NotAFile { path } => ConfigurationIssue::NotFile {
                path: SafePath::new(path),
            },
            Self::Read { path, source } => ConfigurationIssue::Read {
                path: SafePath::new(path),
                failure: IoFailure::from_error(source),
            },
            Self::InvalidUtf8 {
                path,
                valid_up_to,
                error_len,
            } => ConfigurationIssue::InvalidUtf8 {
                path: SafePath::new(path),
                valid_up_to: *valid_up_to,
                error_len: *error_len,
            },
            Self::InvalidToml {
                path,
                location,
                resource,
                failure,
            } => ConfigurationIssue::InvalidToml {
                path: SafePath::new(path),
                line: location.map(SourceLocation::line),
                column: location.map(SourceLocation::column),
                resource: SafeText::new(resource),
                failure: *failure,
            },
            Self::InvalidValue(source) => ConfigurationIssue::InvalidValue {
                path: None,
                field: SafeIdentifier::from_validated(source.field()),
                rule: source.reason().clone(),
            },
            Self::InvalidValueAtPath { path, source } => ConfigurationIssue::InvalidValue {
                path: Some(SafePath::new(path)),
                field: SafeIdentifier::from_validated(source.field()),
                rule: source.reason().clone(),
            },
            Self::TranslationProfileNotFound { path, profile_id } => {
                ConfigurationIssue::TranslationProfileNotFound {
                    path: SafePath::new(path),
                    profile_id: SafeIdentifier::from_validated(profile_id),
                }
            }
            Self::ProfileSelectionConflict {
                path,
                explicit_profile,
                requested_profile,
            } => ConfigurationIssue::ProfileSelectionConflict {
                path: SafePath::new(path),
                explicit_profile: SafeIdentifier::from_validated(explicit_profile),
                requested_profile: SafeIdentifier::from_validated(requested_profile),
            },
        };
        Diagnostic::configuration(issue)
    }
}

impl fmt::Display for ConfigurationLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open { path, source } => {
                write!(formatter, "无法打开配置文件 {}：{source}", path.display())
            }
            Self::NotAFile { path } => {
                write!(formatter, "配置路径不是普通文件：{}", path.display())
            }
            Self::Read { path, source } => {
                write!(formatter, "无法读取配置文件 {}：{source}", path.display())
            }
            Self::InvalidUtf8 {
                path,
                valid_up_to,
                error_len,
            } => {
                write!(
                    formatter,
                    "配置文件 {} 不是有效 UTF-8（有效前缀为 {valid_up_to} 字节，非法序列长度{}）",
                    path.display(),
                    error_len.map_or_else(|| "未知".to_owned(), |length| length.to_string())
                )
            }
            Self::InvalidToml {
                path,
                location,
                resource,
                failure,
            } => {
                write!(formatter, "{}", path.display())?;
                if let Some(location) = location {
                    write!(formatter, ":{}:{}", location.line, location.column)?;
                }
                write!(
                    formatter,
                    "：{resource}：{}",
                    configuration_toml_failure_description(*failure)
                )
            }
            Self::InvalidValue(source) => write!(formatter, "配置值无效：{source}"),
            Self::InvalidValueAtPath { path, source } => {
                write!(
                    formatter,
                    "配置文件 {}：配置值无效：{source}",
                    path.display()
                )
            }
            Self::TranslationProfileNotFound { path, profile_id } => write!(
                formatter,
                "配置文件 {}：translation.profiles 中不存在 ID 为 {profile_id} 的 Profile",
                path.display()
            ),
            Self::ProfileSelectionConflict {
                path,
                explicit_profile,
                requested_profile,
            } => write!(
                formatter,
                "配置文件 {}：命令行已显式选择 Profile {explicit_profile}，不能再改用 {requested_profile}",
                path.display()
            ),
        }
    }
}

impl Error for ConfigurationLoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Open { source, .. } | Self::Read { source, .. } => Some(source),
            Self::InvalidValue(source) | Self::InvalidValueAtPath { source, .. } => Some(source),
            Self::NotAFile { .. }
            | Self::InvalidUtf8 { .. }
            | Self::InvalidToml { .. }
            | Self::TranslationProfileNotFound { .. }
            | Self::ProfileSelectionConflict { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SourceLocation {
    line: usize,
    column: usize,
}

impl SourceLocation {
    #[cfg(test)]
    pub(super) const fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }

    pub(crate) const fn line(self) -> usize {
        self.line
    }

    pub(crate) const fn column(self) -> usize {
        self.column
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConfigurationValueError {
    field: String,
    rule: ConfigurationValueRule,
}

impl ConfigurationValueError {
    /// 返回配置契约中的稳定字段身份。
    pub(crate) fn field(&self) -> &str {
        &self.field
    }

    /// 返回生产校验器在仍持有具体规则时保存的闭集安全原因。
    pub(crate) const fn reason(&self) -> &ConfigurationValueRule {
        &self.rule
    }
}

impl fmt::Display for ConfigurationValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}：{}", self.field, self.rule.render())
    }
}

impl Error for ConfigurationValueError {}

/// 按现实消费范围选择需要建立的配置值不变量。
#[derive(Clone, Copy)]
enum ConfigurationSelection {
    NoAdditionalFields,
    Languages,
    Translate,
    WriteBack,
    SelectedProfile(usize),
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum IndexedTableKind {
    Table,
    TableArray,
}

impl IndexedTableKind {
    const fn expected(self) -> ConfigurationTomlValueKind {
        match self {
            Self::Table => ConfigurationTomlValueKind::Table,
            Self::TableArray => ConfigurationTomlValueKind::TableArray,
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum IndexedScalarKind {
    String,
    Integer,
    Boolean,
    Other,
}

#[derive(Clone)]
enum IndexedValueShape {
    Scalar {
        kind: IndexedScalarKind,
        span: Range<usize>,
    },
    Array {
        items: Vec<IndexedValueShape>,
        span: Range<usize>,
    },
    InlineTable {
        fields: Vec<IndexedInlineField>,
        span: Range<usize>,
    },
}

impl IndexedValueShape {
    fn span(&self) -> &Range<usize> {
        match self {
            Self::Scalar { span, .. }
            | Self::Array { span, .. }
            | Self::InlineTable { span, .. } => span,
        }
    }

    fn matches(&self, expected: ConfigurationTomlValueKind) -> bool {
        match expected {
            ConfigurationTomlValueKind::String => matches!(
                self,
                Self::Scalar {
                    kind: IndexedScalarKind::String,
                    ..
                }
            ),
            ConfigurationTomlValueKind::Integer => matches!(
                self,
                Self::Scalar {
                    kind: IndexedScalarKind::Integer,
                    ..
                }
            ),
            ConfigurationTomlValueKind::Boolean => matches!(
                self,
                Self::Scalar {
                    kind: IndexedScalarKind::Boolean,
                    ..
                }
            ),
            ConfigurationTomlValueKind::StringOrBoolean => matches!(
                self,
                Self::Scalar {
                    kind: IndexedScalarKind::String | IndexedScalarKind::Boolean,
                    ..
                }
            ),
            ConfigurationTomlValueKind::StringArray => {
                self.array_items_match(|item| item.matches(ConfigurationTomlValueKind::String))
            }
            ConfigurationTomlValueKind::IntegerArray => {
                self.array_items_match(|item| item.matches(ConfigurationTomlValueKind::Integer))
            }
            ConfigurationTomlValueKind::Table => {
                matches!(self, Self::InlineTable { .. })
            }
            ConfigurationTomlValueKind::TableArray => false,
        }
    }

    fn array_items_match(&self, predicate: impl Fn(&IndexedValueShape) -> bool) -> bool {
        let Self::Array { items, .. } = self else {
            return false;
        };
        items.iter().all(predicate)
    }
}

#[derive(Clone)]
struct IndexedInlineField {
    path: Vec<String>,
    shape: IndexedValueShape,
    key_span: Range<usize>,
}

#[derive(Clone)]
struct IndexedTable {
    path: Vec<String>,
    kind: IndexedTableKind,
    occurrence: Option<usize>,
    span: Range<usize>,
}

#[derive(Clone)]
struct IndexedField {
    path: Vec<String>,
    table_occurrence: Option<usize>,
    shape: IndexedValueShape,
    key_span: Range<usize>,
}

/// TOML 原文的结构与 span 索引。
///
/// 索引只解码字段名；值正文交给 `()` 作为 decoder sink，仅取得标量/数组/表形态并完成
/// TOML 语法校验。API key、Prompt、parameters 和其他正文不会进入索引或错误对象。
#[derive(Clone)]
struct ConfigurationTomlIndex {
    tables: Vec<IndexedTable>,
    fields: Vec<IndexedField>,
}

impl ConfigurationTomlIndex {
    fn build(source: &str, path: &Path) -> Result<Self, ConfigurationLoadError> {
        let source_view = Source::new(source);
        let tokens = source_view.lex().into_vec();
        let mut events = Vec::<Event>::new();
        let mut errors = Vec::<ParseError>::new();
        {
            let mut validating = ValidateWhitespace::new(&mut events, source_view);
            parse_document(&tokens, &mut validating, &mut errors);
        }

        for event in &events {
            let raw = source_view
                .get(event)
                .expect("toml_parser 事件 span 必须来自同一原文");
            match event.kind() {
                EventKind::SimpleKey => {
                    let mut decoded = String::new();
                    raw.decode_key(&mut decoded, &mut errors);
                }
                EventKind::Scalar => {
                    let _ = raw.decode_scalar(&mut (), &mut errors);
                }
                _ => {}
            }
        }

        let indexed = ConfigurationTomlIndexParser::new(source_view, &events).parse();
        if let Some(offset) = earliest_parse_error_offset(&errors) {
            let resource = match &indexed {
                Ok(index) => index.resource_at(offset),
                Err(failure) if failure.failure == ConfigurationTomlFailureKind::Syntax => {
                    failure.resource.clone()
                }
                Err(_) => "TOML 文档".to_owned(),
            };
            return Err(configuration_toml_failure(
                path,
                source,
                Some(offset..offset),
                resource,
                ConfigurationTomlFailureKind::Syntax,
            ));
        }

        indexed.map_err(|failure| {
            configuration_toml_failure(
                path,
                source,
                Some(failure.span),
                failure.resource,
                failure.failure,
            )
        })
    }

    fn validate_complete_field_set(
        &self,
        source: &str,
        path: &Path,
    ) -> Result<(), ConfigurationLoadError> {
        for table in &self.tables {
            if let Some((value_path, expected)) =
                ConfigurationFieldContract::owning_value_field(&table.path)
            {
                return Err(self.failure_at(
                    source,
                    path,
                    &table.span,
                    value_path,
                    ConfigurationTomlFailureKind::TypeMismatch { expected },
                ));
            }
            let Some(expected) = ConfigurationFieldContract::table_kind(&table.path) else {
                return Err(self.failure_at(
                    source,
                    path,
                    &table.span,
                    &table.path,
                    ConfigurationTomlFailureKind::UnknownField,
                ));
            };
            if table.kind != expected {
                return Err(self.failure_at(
                    source,
                    path,
                    &table.span,
                    &table.path,
                    ConfigurationTomlFailureKind::TypeMismatch {
                        expected: expected.expected(),
                    },
                ));
            }
        }

        for field in &self.fields {
            // 一个本应为标量或数组的字段即使实际写成内联表，内联表成员也不成为
            // 独立配置字段。所选消费范围会在校验该字段时报告其 shape 不符；未选择
            // 的动态 client/profile 仍保持不读取、不校验正文的既有契约。
            if ConfigurationFieldContract::is_descendant_of_value_field(&field.path) {
                continue;
            }
            let expected = ConfigurationFieldContract::field_kind(&field.path)
                .or_else(|| ConfigurationFieldContract::structural_kind(&field.path));
            let Some(expected) = expected else {
                return Err(self.failure_at(
                    source,
                    path,
                    &field.key_span,
                    &field.path,
                    ConfigurationTomlFailureKind::UnknownField,
                ));
            };
            if ConfigurationFieldContract::structural_kind(&field.path).is_some()
                && !field.shape.matches(expected)
            {
                return Err(self.failure_at(
                    source,
                    path,
                    field.shape.span(),
                    &field.path,
                    ConfigurationTomlFailureKind::TypeMismatch { expected },
                ));
            }
        }
        Ok(())
    }

    fn validate_selection(
        &self,
        source: &str,
        path: &Path,
        selection: ConfigurationSelection,
    ) -> Result<(), ConfigurationLoadError> {
        match selection {
            ConfigurationSelection::NoAdditionalFields => {}
            ConfigurationSelection::Languages => self.validate_languages(source, path)?,
            ConfigurationSelection::Translate => self.validate_translate(source, path)?,
            ConfigurationSelection::WriteBack => {}
            ConfigurationSelection::SelectedProfile(occurrence) => {
                for field in ConfigurationFieldContract::PROFILE_REQUIRED_FIELDS {
                    self.require_contract_field(
                        source,
                        path,
                        &["translation", "profiles", field],
                        Some(occurrence),
                    )?;
                }
            }
        }
        Ok(())
    }

    fn validate_selected_client(
        &self,
        source: &str,
        path: &Path,
        client_id: &str,
    ) -> Result<(), ConfigurationLoadError> {
        let mut field_path = vec!["llm", "clients", client_id, ""];
        for field in ConfigurationFieldContract::CLIENT_REQUIRED_FIELDS {
            field_path[3] = field;
            self.require_contract_field(source, path, &field_path, None)?;
        }

        let rate_limit_path = ["llm", "clients", client_id, "rate_limit"];
        if self.has_table(&rate_limit_path, None) {
            for field in ConfigurationFieldContract::RATE_LIMIT_REQUIRED_FIELDS {
                self.require_contract_field(
                    source,
                    path,
                    &["llm", "clients", client_id, "rate_limit", field],
                    None,
                )?;
            }
        }
        Ok(())
    }

    fn validate_translate(&self, source: &str, path: &Path) -> Result<(), ConfigurationLoadError> {
        for field_path in ConfigurationFieldContract::TRANSLATE_REQUIRED_FIELDS {
            self.require_contract_field(source, path, field_path, None)?;
        }
        if self
            .field(&["translation", "record_translation_tasks"], None)
            .is_some()
        {
            self.require_contract_field(
                source,
                path,
                &["translation", "record_translation_tasks"],
                None,
            )?;
        }

        self.validate_languages(source, path)?;

        let profile_tables = self.table_occurrences(&["translation", "profiles"]);
        if profile_tables.is_empty() {
            return Err(self.missing_field(
                source,
                path,
                &["translation", "profiles"],
                None,
                ConfigurationTomlValueKind::TableArray,
            ));
        }
        for occurrence in profile_tables {
            self.require_contract_field(
                source,
                path,
                &["translation", "profiles", "id"],
                Some(occurrence),
            )?;
        }
        Ok(())
    }

    fn validate_languages(&self, source: &str, path: &Path) -> Result<(), ConfigurationLoadError> {
        let language_tables = self.table_occurrences(&["languages"]);
        if language_tables.is_empty() {
            return Err(self.missing_field(
                source,
                path,
                &["languages"],
                None,
                ConfigurationTomlValueKind::TableArray,
            ));
        }
        for occurrence in language_tables {
            for field in ConfigurationFieldContract::LANGUAGE_BASE_REQUIRED_FIELDS {
                self.require_contract_field(source, path, &["languages", field], Some(occurrence))?;
            }
            for field in ConfigurationFieldContract::LANGUAGE_OPTIONAL_FIELDS {
                self.validate_optional_contract_field(
                    source,
                    path,
                    &["languages", field],
                    Some(occurrence),
                )?;
            }
        }
        Ok(())
    }

    fn validate_language_variants(
        &self,
        source: &str,
        path: &Path,
        language_types: &[String],
    ) -> Result<(), ConfigurationLoadError> {
        for (occurrence, language_type) in language_types.iter().enumerate() {
            let required =
                match ConfigurationFieldContract::language_variant_required_fields(language_type) {
                    Some(required) => required,
                    _ => {
                        let field = self
                            .field(&["languages", "type"], Some(occurrence))
                            .expect("Translate 基础 contract 已建立每个语言 type 字段");
                        return Err(self.failure_at(
                            source,
                            path,
                            field.shape.span(),
                            &field.path,
                            ConfigurationTomlFailureKind::InvalidValue,
                        ));
                    }
                };
            for field in required {
                self.require_contract_field(source, path, &["languages", field], Some(occurrence))?;
            }
        }
        Ok(())
    }

    fn require_contract_field(
        &self,
        source: &str,
        path: &Path,
        field_path: &[&str],
        occurrence: Option<usize>,
    ) -> Result<(), ConfigurationLoadError> {
        let expected = ConfigurationFieldContract::expected_field_kind(field_path);
        self.require_field(source, path, field_path, occurrence, expected)
    }

    fn require_field(
        &self,
        source: &str,
        path: &Path,
        field_path: &[&str],
        occurrence: Option<usize>,
        expected: ConfigurationTomlValueKind,
    ) -> Result<(), ConfigurationLoadError> {
        let Some(field) = self.field(field_path, occurrence) else {
            if let Some(descendant) = self.fields.iter().find(|candidate| {
                candidate.table_occurrence == occurrence
                    && field_path.len() < candidate.path.len()
                    && candidate
                        .path
                        .iter()
                        .zip(field_path)
                        .all(|(actual, expected)| actual == expected)
            }) {
                return Err(configuration_toml_failure(
                    path,
                    source,
                    Some(descendant.key_span.clone()),
                    field_path.join("."),
                    ConfigurationTomlFailureKind::TypeMismatch { expected },
                ));
            }
            return Err(self.missing_field(source, path, field_path, occurrence, expected));
        };
        if !field.shape.matches(expected) {
            return Err(self.failure_at(
                source,
                path,
                field.shape.span(),
                &field.path,
                ConfigurationTomlFailureKind::TypeMismatch { expected },
            ));
        }
        Ok(())
    }

    fn validate_optional_contract_field(
        &self,
        source: &str,
        path: &Path,
        field_path: &[&str],
        occurrence: Option<usize>,
    ) -> Result<(), ConfigurationLoadError> {
        if self.field(field_path, occurrence).is_some() {
            self.require_contract_field(source, path, field_path, occurrence)?;
        }
        Ok(())
    }

    fn field(&self, path: &[&str], occurrence: Option<usize>) -> Option<&IndexedField> {
        self.fields.iter().find(|field| {
            field.table_occurrence == occurrence
                && field.path.len() == path.len()
                && field
                    .path
                    .iter()
                    .zip(path)
                    .all(|(actual, expected)| actual == expected)
        })
    }

    fn has_table(&self, path: &[&str], occurrence: Option<usize>) -> bool {
        self.tables.iter().any(|table| {
            table.occurrence == occurrence
                && table.path.len() == path.len()
                && table
                    .path
                    .iter()
                    .zip(path)
                    .all(|(actual, expected)| actual == expected)
        }) || self
            .field(path, occurrence)
            .is_some_and(|field| field.shape.matches(ConfigurationTomlValueKind::Table))
    }

    fn table_occurrences(&self, path: &[&str]) -> Vec<usize> {
        self.tables
            .iter()
            .filter(|table| {
                table.kind == IndexedTableKind::TableArray
                    && table.path.len() == path.len()
                    && table
                        .path
                        .iter()
                        .zip(path)
                        .all(|(actual, expected)| actual == expected)
            })
            .filter_map(|table| table.occurrence)
            .collect()
    }

    fn missing_field(
        &self,
        source: &str,
        path: &Path,
        field_path: &[&str],
        occurrence: Option<usize>,
        _expected: ConfigurationTomlValueKind,
    ) -> ConfigurationLoadError {
        let owner = &field_path[..field_path.len().saturating_sub(1)];
        let span = self
            .tables
            .iter()
            .find(|table| {
                table.occurrence == occurrence
                    && table.path.len() == owner.len()
                    && table
                        .path
                        .iter()
                        .zip(owner)
                        .all(|(actual, expected)| actual == expected)
            })
            .map(|table| table.span.clone());
        configuration_toml_failure(
            path,
            source,
            span,
            field_path.join("."),
            ConfigurationTomlFailureKind::MissingField,
        )
    }

    fn failure_at(
        &self,
        source: &str,
        path: &Path,
        span: &Range<usize>,
        resource: &[String],
        failure: ConfigurationTomlFailureKind,
    ) -> ConfigurationLoadError {
        configuration_toml_failure(
            path,
            source,
            Some(span.clone()),
            resource.join("."),
            failure,
        )
    }

    fn resource_at(&self, offset: usize) -> String {
        self.fields
            .iter()
            .filter(|field| field.key_span.start <= offset || field.shape.span().start <= offset)
            .max_by_key(|field| field.key_span.start.max(field.shape.span().start))
            .map_or_else(|| "TOML 文档".to_owned(), |field| field.path.join("."))
    }
}

struct ConfigurationFieldContract;

impl ConfigurationFieldContract {
    const TRANSLATE_REQUIRED_FIELDS: &'static [&'static [&'static str]] =
        &[&["prompts", "thinking_output"], &["prompts", "source_echo"]];
    const LANGUAGE_BASE_REQUIRED_FIELDS: &'static [&'static str] = &["type", "id"];
    const LANGUAGE_OPTIONAL_FIELDS: &'static [&'static str] = &[
        "minimum_kana_characters",
        "minimum_word_count",
        "minimum_letter_count",
        "minimum_copied_word_count",
        "minimum_copied_letter_count",
        "allowed_terms",
        "ignored_terms",
    ];
    const JAPANESE_REQUIRED_FIELDS: &'static [&'static str] =
        &["minimum_kana_characters", "allowed_terms"];
    const ENGLISH_REQUIRED_FIELDS: &'static [&'static str] = &[
        "minimum_word_count",
        "minimum_letter_count",
        "ignored_terms",
        "minimum_copied_word_count",
        "minimum_copied_letter_count",
        "allowed_terms",
    ];
    const PROFILE_REQUIRED_FIELDS: &'static [&'static str] =
        &["id", "llm_client", "target_task_user_message_characters"];
    const CLIENT_REQUIRED_FIELDS: &'static [&'static str] = &[
        "url",
        "api_key",
        "model",
        "stream",
        "max_concurrent_requests",
        "connect_timeout_ms",
        "read_timeout_ms",
        "request_timeout_ms",
        "proxy",
        "additional_pem_files",
        "retry_delays_ms",
        "max_retry_after_ms",
        "parameters",
    ];
    const RATE_LIMIT_REQUIRED_FIELDS: &'static [&'static str] = &["requests_per_minute", "burst"];

    fn language_variant_required_fields(language_type: &str) -> Option<&'static [&'static str]> {
        match language_type {
            "japanese" => Some(Self::JAPANESE_REQUIRED_FIELDS),
            "english" => Some(Self::ENGLISH_REQUIRED_FIELDS),
            _ => None,
        }
    }

    fn expected_field_kind(path: &[&str]) -> ConfigurationTomlValueKind {
        let path = path
            .iter()
            .map(|part| (*part).to_owned())
            .collect::<Vec<_>>();
        Self::field_kind(&path).expect("必填或可选字段必须由唯一配置字段契约声明")
    }

    fn owning_value_field(path: &[String]) -> Option<(&[String], ConfigurationTomlValueKind)> {
        (1..=path.len()).find_map(|prefix_len| {
            let prefix = &path[..prefix_len];
            Self::field_kind(prefix).map(|expected| (prefix, expected))
        })
    }

    fn is_descendant_of_value_field(path: &[String]) -> bool {
        path.len() > 1 && Self::owning_value_field(&path[..path.len() - 1]).is_some()
    }

    fn table_kind(path: &[String]) -> Option<IndexedTableKind> {
        match path {
            [first]
                if matches!(
                    first.as_str(),
                    "prompts" | "llm" | "translation" | "write_back"
                ) =>
            {
                Some(IndexedTableKind::Table)
            }
            [llm, clients] if llm == "llm" && clients == "clients" => Some(IndexedTableKind::Table),
            [llm, clients, _] if llm == "llm" && clients == "clients" => {
                Some(IndexedTableKind::Table)
            }
            [llm, clients, _, rate_limit]
                if llm == "llm" && clients == "clients" && rate_limit == "rate_limit" =>
            {
                Some(IndexedTableKind::Table)
            }
            [languages] if languages == "languages" => Some(IndexedTableKind::TableArray),
            [translation, profiles] if translation == "translation" && profiles == "profiles" => {
                Some(IndexedTableKind::TableArray)
            }
            _ => None,
        }
    }

    fn structural_kind(path: &[String]) -> Option<ConfigurationTomlValueKind> {
        Self::table_kind(path).map(IndexedTableKind::expected)
    }

    fn field_kind(path: &[String]) -> Option<ConfigurationTomlValueKind> {
        let kind = match path {
            [prompts, field]
                if prompts == "prompts"
                    && matches!(field.as_str(), "thinking_output" | "source_echo") =>
            {
                ConfigurationTomlValueKind::Boolean
            }
            [llm, clients, _, field]
                if llm == "llm"
                    && clients == "clients"
                    && matches!(
                        field.as_str(),
                        "url" | "protocol" | "api_key" | "model" | "parameters"
                    ) =>
            {
                ConfigurationTomlValueKind::String
            }
            [llm, clients, _, stream]
                if llm == "llm" && clients == "clients" && stream == "stream" =>
            {
                ConfigurationTomlValueKind::Boolean
            }
            [llm, clients, _, field]
                if llm == "llm"
                    && clients == "clients"
                    && matches!(
                        field.as_str(),
                        "max_concurrent_requests"
                            | "connect_timeout_ms"
                            | "read_timeout_ms"
                            | "request_timeout_ms"
                            | "max_retry_after_ms"
                    ) =>
            {
                ConfigurationTomlValueKind::Integer
            }
            [llm, clients, _, proxy]
                if llm == "llm" && clients == "clients" && proxy == "proxy" =>
            {
                ConfigurationTomlValueKind::StringOrBoolean
            }
            [llm, clients, _, pem]
                if llm == "llm" && clients == "clients" && pem == "additional_pem_files" =>
            {
                ConfigurationTomlValueKind::StringArray
            }
            [llm, clients, _, delays]
                if llm == "llm" && clients == "clients" && delays == "retry_delays_ms" =>
            {
                ConfigurationTomlValueKind::IntegerArray
            }
            [llm, clients, _, rate_limit, field]
                if llm == "llm"
                    && clients == "clients"
                    && rate_limit == "rate_limit"
                    && matches!(field.as_str(), "requests_per_minute" | "burst") =>
            {
                ConfigurationTomlValueKind::Integer
            }
            [languages, field]
                if languages == "languages" && matches!(field.as_str(), "type" | "id") =>
            {
                ConfigurationTomlValueKind::String
            }
            [languages, field]
                if languages == "languages"
                    && matches!(
                        field.as_str(),
                        "minimum_kana_characters"
                            | "minimum_word_count"
                            | "minimum_letter_count"
                            | "minimum_copied_word_count"
                            | "minimum_copied_letter_count"
                    ) =>
            {
                ConfigurationTomlValueKind::Integer
            }
            [languages, field]
                if languages == "languages"
                    && matches!(field.as_str(), "allowed_terms" | "ignored_terms") =>
            {
                ConfigurationTomlValueKind::StringArray
            }
            [translation, record]
                if translation == "translation" && record == "record_translation_tasks" =>
            {
                ConfigurationTomlValueKind::Boolean
            }
            [write_back, field]
                if write_back == "write_back"
                    && matches!(
                        field.as_str(),
                        "repair_punctuation" | "complete_continuation_whitespace"
                    ) =>
            {
                ConfigurationTomlValueKind::Boolean
            }
            [translation, profiles, field]
                if translation == "translation"
                    && profiles == "profiles"
                    && matches!(field.as_str(), "id" | "llm_client") =>
            {
                ConfigurationTomlValueKind::String
            }
            [translation, profiles, target]
                if translation == "translation"
                    && profiles == "profiles"
                    && target == "target_task_user_message_characters" =>
            {
                ConfigurationTomlValueKind::Integer
            }
            _ => return None,
        };
        Some(kind)
    }
}

struct IndexedBuildFailure {
    span: Range<usize>,
    resource: String,
    failure: ConfigurationTomlFailureKind,
}

struct ConfigurationTomlIndexParser<'a> {
    source: Source<'a>,
    events: &'a [Event],
    cursor: usize,
    current_table: Vec<String>,
    current_occurrence: Option<usize>,
    current_assignment: Option<Vec<String>>,
    table_occurrences: HashMap<Vec<String>, usize>,
    declared_tables: BTreeSet<Vec<String>>,
    assigned_fields: BTreeSet<(Vec<String>, Option<usize>)>,
    tables: Vec<IndexedTable>,
    fields: Vec<IndexedField>,
}

impl<'a> ConfigurationTomlIndexParser<'a> {
    fn new(source: Source<'a>, events: &'a [Event]) -> Self {
        Self {
            source,
            events,
            cursor: 0,
            current_table: Vec::new(),
            current_occurrence: None,
            current_assignment: None,
            table_occurrences: HashMap::new(),
            declared_tables: BTreeSet::new(),
            assigned_fields: BTreeSet::new(),
            tables: Vec::new(),
            fields: Vec::new(),
        }
    }

    fn parse(mut self) -> Result<ConfigurationTomlIndex, IndexedBuildFailure> {
        while self.skip_document_trivia() {
            match self.peek_kind() {
                Some(EventKind::StdTableOpen | EventKind::ArrayTableOpen) => {
                    self.parse_table_header()?;
                }
                Some(EventKind::SimpleKey) => self.parse_assignment()?,
                Some(_) => return Err(self.syntax_failure(self.current_span())),
                None => break,
            }
        }
        Ok(ConfigurationTomlIndex {
            tables: self.tables,
            fields: self.fields,
        })
    }

    fn parse_table_header(&mut self) -> Result<(), IndexedBuildFailure> {
        let open = self.next().expect("已确认存在表头开始事件");
        let (kind, close_kind) = match open.kind() {
            EventKind::StdTableOpen => (IndexedTableKind::Table, EventKind::StdTableClose),
            EventKind::ArrayTableOpen => (IndexedTableKind::TableArray, EventKind::ArrayTableClose),
            _ => unreachable!("调用方只在表头事件调用"),
        };
        let (table_path, key_span) = self.parse_key_path_until(close_kind)?;
        if table_path.is_empty() {
            return Err(self.syntax_failure(span_range(open.span())));
        }
        let close = self.next().expect("键路径解析必须停在表头结束事件");
        let span = open.span().start()..close.span().end();

        let conflicts_with_value =
            self.assigned_fields
                .iter()
                .any(|(assigned_path, assigned_occurrence)| {
                    !(kind == IndexedTableKind::TableArray && assigned_occurrence.is_some())
                        && value_paths_conflict(assigned_path, &table_path)
                });
        let conflicts_with_table = self.tables.iter().any(|declared| {
            if declared.path == table_path {
                kind != IndexedTableKind::TableArray
                    || declared.kind != IndexedTableKind::TableArray
            } else {
                path_is_prefix(&table_path, &declared.path)
            }
        });
        if conflicts_with_value || conflicts_with_table {
            return Err(IndexedBuildFailure {
                span: key_span,
                resource: table_path.join("."),
                failure: ConfigurationTomlFailureKind::DuplicateField,
            });
        }

        let occurrence = if kind == IndexedTableKind::TableArray {
            let next = self
                .table_occurrences
                .entry(table_path.clone())
                .or_default();
            let occurrence = *next;
            *next += 1;
            Some(occurrence)
        } else {
            let inserted = self.declared_tables.insert(table_path.clone());
            debug_assert!(inserted, "重复显式表必须已由结构冲突检查拒绝");
            None
        };
        self.current_table.clone_from(&table_path);
        self.current_occurrence = occurrence;
        self.tables.push(IndexedTable {
            path: table_path,
            kind,
            occurrence,
            span,
        });
        Ok(())
    }

    fn parse_assignment(&mut self) -> Result<(), IndexedBuildFailure> {
        let (local_path, key_span) = self.parse_key_path_until(EventKind::KeyValSep)?;
        if local_path.is_empty() {
            return Err(self.syntax_failure(key_span));
        }
        let _separator = self.next().expect("键路径解析必须停在等号事件");
        let mut path = self.current_table.clone();
        path.extend(local_path);
        self.current_assignment = Some(path.clone());
        let shape = self.parse_value()?;
        let identity = (path.clone(), self.current_occurrence);
        let conflicts_with_value =
            self.assigned_fields
                .iter()
                .any(|(assigned_path, assigned_occurrence)| {
                    *assigned_occurrence == self.current_occurrence
                        && value_paths_conflict(assigned_path, &path)
                });
        let conflicts_with_table = self
            .tables
            .iter()
            .any(|table| path_is_prefix(&path, &table.path));
        if conflicts_with_value || conflicts_with_table {
            return Err(IndexedBuildFailure {
                span: key_span,
                resource: path.join("."),
                failure: ConfigurationTomlFailureKind::DuplicateField,
            });
        }
        let inserted = self.assigned_fields.insert(identity);
        debug_assert!(inserted, "重复赋值必须已由结构冲突检查拒绝");
        self.fields.push(IndexedField {
            path: path.clone(),
            table_occurrence: self.current_occurrence,
            shape: shape.clone(),
            key_span: key_span.clone(),
        });
        self.flatten_inline_fields(&path, self.current_occurrence, &shape)?;
        Ok(())
    }

    fn flatten_inline_fields(
        &mut self,
        prefix: &[String],
        occurrence: Option<usize>,
        shape: &IndexedValueShape,
    ) -> Result<(), IndexedBuildFailure> {
        let IndexedValueShape::InlineTable { fields, .. } = shape else {
            return Ok(());
        };
        for inline in fields {
            let mut path = prefix.to_vec();
            path.extend(inline.path.iter().cloned());
            if !self.assigned_fields.insert((path.clone(), occurrence)) {
                return Err(IndexedBuildFailure {
                    span: inline.key_span.clone(),
                    resource: path.join("."),
                    failure: ConfigurationTomlFailureKind::DuplicateField,
                });
            }
            self.fields.push(IndexedField {
                path: path.clone(),
                table_occurrence: occurrence,
                shape: inline.shape.clone(),
                key_span: inline.key_span.clone(),
            });
            self.flatten_inline_fields(&path, occurrence, &inline.shape)?;
        }
        Ok(())
    }

    fn parse_key_path_until(
        &mut self,
        terminator: EventKind,
    ) -> Result<(Vec<String>, Range<usize>), IndexedBuildFailure> {
        let mut path = Vec::new();
        let mut start = None;
        let mut end = None;
        loop {
            self.skip_trivia();
            if self.peek_kind() == Some(terminator) {
                let point = start.unwrap_or_else(|| self.current_span().start);
                return Ok((path, point..end.unwrap_or(point)));
            }
            let Some(event) = self.next() else {
                return Err(self.syntax_failure(self.current_span()));
            };
            if event.kind() != EventKind::SimpleKey {
                return Err(self.syntax_failure(span_range(event.span())));
            }
            let raw = self
                .source
                .get(event)
                .expect("键事件 span 必须来自同一原文");
            let mut key = String::new();
            let mut errors = Vec::new();
            raw.decode_key(&mut key, &mut errors);
            if !errors.is_empty() {
                return Err(self.syntax_failure(span_range(event.span())));
            }
            start.get_or_insert(event.span().start());
            end = Some(event.span().end());
            path.push(key);
            self.skip_trivia();
            if self.peek_kind() == Some(EventKind::KeySep) {
                let _ = self.next();
                continue;
            }
            if self.peek_kind() != Some(terminator) {
                return Err(self.syntax_failure(self.current_span()));
            }
        }
    }

    fn parse_value(&mut self) -> Result<IndexedValueShape, IndexedBuildFailure> {
        self.skip_trivia();
        let Some(event) = self.next() else {
            return Err(self.syntax_failure(self.current_span()));
        };
        match event.kind() {
            EventKind::Scalar => {
                let raw = self
                    .source
                    .get(event)
                    .expect("值事件 span 必须来自同一原文");
                let mut errors = Vec::new();
                let kind = match raw.decode_scalar(&mut (), &mut errors) {
                    ScalarKind::String => IndexedScalarKind::String,
                    ScalarKind::Integer(_) => IndexedScalarKind::Integer,
                    ScalarKind::Boolean(_) => IndexedScalarKind::Boolean,
                    ScalarKind::DateTime | ScalarKind::Float => IndexedScalarKind::Other,
                };
                if !errors.is_empty() {
                    return Err(self.syntax_failure(span_range(event.span())));
                }
                Ok(IndexedValueShape::Scalar {
                    kind,
                    span: span_range(event.span()),
                })
            }
            EventKind::ArrayOpen => self.parse_array(event.span()),
            EventKind::InlineTableOpen => self.parse_inline_table(event.span()),
            _ => Err(self.syntax_failure(span_range(event.span()))),
        }
    }

    fn parse_array(&mut self, open: Span) -> Result<IndexedValueShape, IndexedBuildFailure> {
        let mut items = Vec::new();
        loop {
            self.skip_trivia();
            if self.peek_kind() == Some(EventKind::ArrayClose) {
                let close = self.next().expect("已确认数组结束事件存在");
                return Ok(IndexedValueShape::Array {
                    items,
                    span: open.start()..close.span().end(),
                });
            }
            items.push(self.parse_value()?);
            self.skip_trivia();
            if self.peek_kind() == Some(EventKind::ValueSep) {
                let _ = self.next();
                continue;
            }
            if self.peek_kind() != Some(EventKind::ArrayClose) {
                return Err(self.syntax_failure(self.current_span()));
            }
        }
    }

    fn parse_inline_table(&mut self, open: Span) -> Result<IndexedValueShape, IndexedBuildFailure> {
        let mut fields = Vec::new();
        let mut seen = BTreeSet::<Vec<String>>::new();
        loop {
            self.skip_trivia();
            if self.peek_kind() == Some(EventKind::InlineTableClose) {
                let close = self.next().expect("已确认内联表结束事件存在");
                return Ok(IndexedValueShape::InlineTable {
                    fields,
                    span: open.start()..close.span().end(),
                });
            }
            let (path, key_span) = self.parse_key_path_until(EventKind::KeyValSep)?;
            let _separator = self.next().expect("内联表字段必须拥有等号");
            let shape = self.parse_value()?;
            if seen
                .iter()
                .any(|existing| value_paths_conflict(existing, &path))
            {
                return Err(IndexedBuildFailure {
                    span: key_span,
                    resource: path.join("."),
                    failure: ConfigurationTomlFailureKind::DuplicateField,
                });
            }
            let inserted = seen.insert(path.clone());
            debug_assert!(inserted, "重复内联字段必须已由结构冲突检查拒绝");
            fields.push(IndexedInlineField {
                path,
                shape,
                key_span,
            });
            self.skip_trivia();
            if self.peek_kind() == Some(EventKind::ValueSep) {
                let _ = self.next();
                continue;
            }
            if self.peek_kind() != Some(EventKind::InlineTableClose) {
                return Err(self.syntax_failure(self.current_span()));
            }
        }
    }

    fn skip_trivia(&mut self) -> bool {
        while matches!(
            self.peek_kind(),
            Some(EventKind::Whitespace | EventKind::Comment | EventKind::Newline)
        ) {
            self.cursor += 1;
        }
        self.cursor < self.events.len()
    }

    fn skip_document_trivia(&mut self) -> bool {
        while matches!(
            self.peek_kind(),
            Some(EventKind::Whitespace | EventKind::Comment | EventKind::Newline)
        ) {
            if self.peek_kind() == Some(EventKind::Newline) {
                self.current_assignment = None;
            }
            self.cursor += 1;
        }
        self.cursor < self.events.len()
    }

    fn peek_kind(&self) -> Option<EventKind> {
        self.events.get(self.cursor).map(Event::kind)
    }

    fn next(&mut self) -> Option<Event> {
        let event = self.events.get(self.cursor).copied()?;
        self.cursor += 1;
        Some(event)
    }

    fn current_span(&self) -> Range<usize> {
        self.events
            .get(self.cursor)
            .map(|event| span_range(event.span()))
            .unwrap_or_else(|| {
                let end = self.source.input().len();
                end..end
            })
    }

    fn syntax_failure(&self, span: Range<usize>) -> IndexedBuildFailure {
        IndexedBuildFailure {
            span,
            resource: self.current_assignment.as_ref().map_or_else(
                || {
                    if self.current_table.is_empty() {
                        "TOML 文档".to_owned()
                    } else {
                        self.current_table.join(".")
                    }
                },
                |path| path.join("."),
            ),
            failure: ConfigurationTomlFailureKind::Syntax,
        }
    }
}

fn path_is_prefix(prefix: &[String], path: &[String]) -> bool {
    prefix.len() <= path.len()
        && prefix
            .iter()
            .zip(path)
            .all(|(expected, actual)| expected == actual)
}

fn value_paths_conflict(left: &[String], right: &[String]) -> bool {
    path_is_prefix(left, right) || path_is_prefix(right, left)
}

fn earliest_parse_error_offset(errors: &[ParseError]) -> Option<usize> {
    errors
        .iter()
        .filter_map(|error| error.unexpected().or_else(|| error.context()))
        .map(|span| span.start())
        .min()
        .or_else(|| (!errors.is_empty()).then_some(0))
}

fn span_range(span: Span) -> Range<usize> {
    span.start()..span.end()
}

fn configuration_toml_failure(
    path: &Path,
    source: &str,
    span: Option<Range<usize>>,
    resource: String,
    failure: ConfigurationTomlFailureKind,
) -> ConfigurationLoadError {
    let location = span
        .as_ref()
        .map(|span| source_location(source, span.start));
    ConfigurationLoadError::InvalidToml {
        path: path.to_path_buf(),
        location,
        resource,
        failure,
    }
}

fn invalid_toml(
    path: &Path,
    source: &str,
    index: &ConfigurationTomlIndex,
    error: &toml::de::Error,
) -> ConfigurationLoadError {
    let span = error.span();
    let resource = span.as_ref().map_or_else(
        || "TOML 文档".to_owned(),
        |span| index.resource_at(span.start),
    );
    configuration_toml_failure(
        path,
        source,
        span,
        resource,
        ConfigurationTomlFailureKind::InvalidValue,
    )
}

fn parse_selected<T>(
    source: &str,
    path: &Path,
    index: &ConfigurationTomlIndex,
    selection: ConfigurationSelection,
) -> Result<T, ConfigurationLoadError>
where
    T: serde::de::DeserializeOwned,
{
    index.validate_selection(source, path, selection)?;
    if matches!(
        selection,
        ConfigurationSelection::Languages | ConfigurationSelection::Translate
    ) {
        let discriminators: RawLanguageDiscriminatorSelection =
            toml::from_str(source).map_err(|error| invalid_toml(path, source, index, &error))?;
        let language_types = discriminators
            .languages
            .into_iter()
            .map(|language| language.language_type)
            .collect::<Vec<_>>();
        index.validate_language_variants(source, path, &language_types)?;
    }
    toml::from_str(source).map_err(|error| invalid_toml(path, source, index, &error))
}

fn configuration_toml_failure_description(failure: ConfigurationTomlFailureKind) -> &'static str {
    match failure {
        ConfigurationTomlFailureKind::Syntax => "TOML 语法无效",
        ConfigurationTomlFailureKind::MissingField => "缺少必填字段",
        ConfigurationTomlFailureKind::UnknownField => "当前配置契约不接受该字段",
        ConfigurationTomlFailureKind::DuplicateField => "字段重复",
        ConfigurationTomlFailureKind::TypeMismatch { .. } => "字段类型不符合当前配置契约",
        ConfigurationTomlFailureKind::InvalidValue => "字段值不符合当前配置契约",
    }
}

fn parse_selected_translation_profile(
    source: &str,
    path: &Path,
    index: &ConfigurationTomlIndex,
    requested_id: &str,
) -> Result<RawSelectedTranslationProfileConfiguration, ConfigurationLoadError> {
    let index_deserializer = toml::de::Deserializer::parse(source)
        .map_err(|error| invalid_toml(path, source, index, &error))?;
    let selection = TranslationProfileIndexTopSeed { requested_id }
        .deserialize(index_deserializer)
        .map_err(|error| invalid_toml(path, source, index, &error))?
        .unwrap_or_default();
    if selection.duplicate {
        return Err(ConfigurationLoadError::InvalidValue(invalid(
            "translation.profiles",
            ConfigurationValueRule::DuplicateProfileId,
        )));
    }
    let selected_index = selection.selected_index.ok_or_else(|| {
        ConfigurationLoadError::TranslationProfileNotFound {
            path: path.to_path_buf(),
            profile_id: requested_id.to_owned(),
        }
    })?;
    index.validate_selection(
        source,
        path,
        ConfigurationSelection::SelectedProfile(selected_index),
    )?;

    let profile_deserializer = toml::de::Deserializer::parse(source)
        .map_err(|error| invalid_toml(path, source, index, &error))?;
    SelectedTranslationProfileTopSeed { selected_index }
        .deserialize(profile_deserializer)
        .map_err(|error| invalid_toml(path, source, index, &error))?
        .ok_or_else(|| {
            ConfigurationLoadError::InvalidValue(invalid(
                "translation.profiles",
                ConfigurationValueRule::SelectedProfileInvalid,
            ))
        })
}

#[derive(Default)]
struct TranslationProfileIndexSelection {
    selected_index: Option<usize>,
    duplicate: bool,
}

struct TranslationProfileIndexTopSeed<'a> {
    requested_id: &'a str,
}

impl<'de> DeserializeSeed<'de> for TranslationProfileIndexTopSeed<'_> {
    type Value = Option<TranslationProfileIndexSelection>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(TranslationProfileIndexTopVisitor {
            requested_id: self.requested_id,
        })
    }
}

struct TranslationProfileIndexTopVisitor<'a> {
    requested_id: &'a str,
}

impl<'de> Visitor<'de> for TranslationProfileIndexTopVisitor<'_> {
    type Value = Option<TranslationProfileIndexSelection>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ATT 顶层 TOML 配置")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut seen_translation = false;
        let mut selection = None;
        while let Some(key) = map.next_key::<String>()? {
            if key == "translation" {
                if seen_translation {
                    return Err(de::Error::duplicate_field("translation"));
                }
                seen_translation = true;
                selection = map.next_value_seed(TranslationProfileIndexTranslationSeed {
                    requested_id: self.requested_id,
                })?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(selection)
    }
}

struct TranslationProfileIndexTranslationSeed<'a> {
    requested_id: &'a str,
}

impl<'de> DeserializeSeed<'de> for TranslationProfileIndexTranslationSeed<'_> {
    type Value = Option<TranslationProfileIndexSelection>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(TranslationProfileIndexTranslationVisitor {
            requested_id: self.requested_id,
        })
    }
}

struct TranslationProfileIndexTranslationVisitor<'a> {
    requested_id: &'a str,
}

impl<'de> Visitor<'de> for TranslationProfileIndexTranslationVisitor<'_> {
    type Value = Option<TranslationProfileIndexSelection>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("公共翻译配置")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut seen_profiles = false;
        let mut selection = None;
        while let Some(key) = map.next_key::<String>()? {
            if key == "profiles" {
                if seen_profiles {
                    return Err(de::Error::duplicate_field("profiles"));
                }
                seen_profiles = true;
                selection = Some(map.next_value_seed(TranslationProfileIndexSequenceSeed {
                    requested_id: self.requested_id,
                })?);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(selection)
    }
}

struct TranslationProfileIndexSequenceSeed<'a> {
    requested_id: &'a str,
}

impl<'de> DeserializeSeed<'de> for TranslationProfileIndexSequenceSeed<'_> {
    type Value = TranslationProfileIndexSelection;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(TranslationProfileIndexSequenceVisitor {
            requested_id: self.requested_id,
        })
    }
}

struct TranslationProfileIndexSequenceVisitor<'a> {
    requested_id: &'a str,
}

impl<'de> Visitor<'de> for TranslationProfileIndexSequenceVisitor<'_> {
    type Value = TranslationProfileIndexSelection;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("公共翻译 Profile 数组")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut selection = TranslationProfileIndexSelection::default();
        let mut index = 0usize;
        while let Some(id) = sequence.next_element_seed(TranslationProfileIdSeed)? {
            if id.as_deref() == Some(self.requested_id)
                && selection.selected_index.replace(index).is_some()
            {
                selection.duplicate = true;
            }
            index += 1;
        }
        Ok(selection)
    }
}

struct TranslationProfileIdSeed;

impl<'de> DeserializeSeed<'de> for TranslationProfileIdSeed {
    type Value = Option<String>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(TranslationProfileIdVisitor)
    }
}

struct TranslationProfileIdVisitor;

impl<'de> Visitor<'de> for TranslationProfileIdVisitor {
    type Value = Option<String>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("只读取 id 的公共翻译 Profile")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut seen_id = false;
        let mut id = None;
        while let Some(key) = map.next_key::<String>()? {
            if key == "id" {
                if seen_id {
                    return Err(de::Error::duplicate_field("id"));
                }
                seen_id = true;
                id = map.next_value_seed(OptionalStringSeed)?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(id)
    }
}

struct OptionalStringSeed;

impl<'de> DeserializeSeed<'de> for OptionalStringSeed {
    type Value = Option<String>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(OptionalStringVisitor)
    }
}

struct OptionalStringVisitor;

impl<'de> Visitor<'de> for OptionalStringVisitor {
    type Value = Option<String>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("字符串或未选择的任意 TOML 值")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Some(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(Some(value))
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence.next_element::<IgnoredAny>()?.is_some() {}
        Ok(None)
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(None)
    }
}

struct SelectedTranslationProfileTopSeed {
    selected_index: usize,
}

impl<'de> DeserializeSeed<'de> for SelectedTranslationProfileTopSeed {
    type Value = Option<RawSelectedTranslationProfileConfiguration>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(SelectedTranslationProfileTopVisitor {
            selected_index: self.selected_index,
        })
    }
}

struct SelectedTranslationProfileTopVisitor {
    selected_index: usize,
}

impl<'de> Visitor<'de> for SelectedTranslationProfileTopVisitor {
    type Value = Option<RawSelectedTranslationProfileConfiguration>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ATT 顶层 TOML 配置")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut seen_translation = false;
        let mut selected = None;
        while let Some(key) = map.next_key::<String>()? {
            if key == "translation" {
                if seen_translation {
                    return Err(de::Error::duplicate_field("translation"));
                }
                seen_translation = true;
                selected = map.next_value_seed(SelectedTranslationProfileTranslationSeed {
                    selected_index: self.selected_index,
                })?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(selected)
    }
}

struct SelectedTranslationProfileTranslationSeed {
    selected_index: usize,
}

impl<'de> DeserializeSeed<'de> for SelectedTranslationProfileTranslationSeed {
    type Value = Option<RawSelectedTranslationProfileConfiguration>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(SelectedTranslationProfileTranslationVisitor {
            selected_index: self.selected_index,
        })
    }
}

struct SelectedTranslationProfileTranslationVisitor {
    selected_index: usize,
}

impl<'de> Visitor<'de> for SelectedTranslationProfileTranslationVisitor {
    type Value = Option<RawSelectedTranslationProfileConfiguration>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("公共翻译配置")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut seen_profiles = false;
        let mut selected = None;
        while let Some(key) = map.next_key::<String>()? {
            if key == "profiles" {
                if seen_profiles {
                    return Err(de::Error::duplicate_field("profiles"));
                }
                seen_profiles = true;
                selected = map.next_value_seed(SelectedTranslationProfileSequenceSeed {
                    selected_index: self.selected_index,
                })?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(selected)
    }
}

struct SelectedTranslationProfileSequenceSeed {
    selected_index: usize,
}

impl<'de> DeserializeSeed<'de> for SelectedTranslationProfileSequenceSeed {
    type Value = Option<RawSelectedTranslationProfileConfiguration>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(SelectedTranslationProfileSequenceVisitor {
            selected_index: self.selected_index,
        })
    }
}

struct SelectedTranslationProfileSequenceVisitor {
    selected_index: usize,
}

impl<'de> Visitor<'de> for SelectedTranslationProfileSequenceVisitor {
    type Value = Option<RawSelectedTranslationProfileConfiguration>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("translation profile 数组")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut selected = None;
        let mut index = 0usize;
        while let Some(profile) =
            sequence.next_element_seed(SelectedTranslationProfileCandidateSeed {
                selected: index == self.selected_index,
            })?
        {
            if profile.is_some() {
                selected = profile;
            }
            index += 1;
        }
        Ok(selected)
    }
}

struct SelectedTranslationProfileCandidateSeed {
    selected: bool,
}

impl<'de> DeserializeSeed<'de> for SelectedTranslationProfileCandidateSeed {
    type Value = Option<RawSelectedTranslationProfileConfiguration>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        if self.selected {
            RawSelectedTranslationProfileConfiguration::deserialize(deserializer).map(Some)
        } else {
            IgnoredAny::deserialize(deserializer).map(|_| None)
        }
    }
}

fn parse_selected_llm_client(
    source: &str,
    path: &Path,
    index: &ConfigurationTomlIndex,
    requested_id: &str,
) -> Result<RawLlmClientConfiguration, ConfigurationLoadError> {
    validate_exact_identifier("llm client id", requested_id)
        .map_err(ConfigurationLoadError::InvalidValue)?;
    if !index.has_table(&["llm", "clients", requested_id], None) {
        return Err(ConfigurationLoadError::InvalidValue(invalid(
            "llm.clients",
            ConfigurationValueRule::ReferencedClientNotFound,
        )));
    }
    index.validate_selected_client(source, path, requested_id)?;
    let seed = SelectedLlmClientTopSeed { requested_id };
    let deserializer = toml::de::Deserializer::parse(source)
        .map_err(|error| invalid_toml(path, source, index, &error))?;
    let selected = seed
        .deserialize(deserializer)
        .map_err(|error| invalid_toml(path, source, index, &error))?;
    selected.ok_or_else(|| {
        ConfigurationLoadError::InvalidValue(invalid(
            "llm.clients",
            ConfigurationValueRule::ReferencedClientNotFound,
        ))
    })
}

struct SelectedLlmClientTopSeed<'a> {
    requested_id: &'a str,
}

impl<'de> DeserializeSeed<'de> for SelectedLlmClientTopSeed<'_> {
    type Value = Option<RawLlmClientConfiguration>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(SelectedLlmClientTopVisitor {
            requested_id: self.requested_id,
        })
    }
}

struct SelectedLlmClientTopVisitor<'a> {
    requested_id: &'a str,
}

impl<'de> Visitor<'de> for SelectedLlmClientTopVisitor<'_> {
    type Value = Option<RawLlmClientConfiguration>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ATT 顶层 TOML 配置")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut selected = None;
        while let Some(key) = map.next_key::<String>()? {
            if key == "llm" {
                if selected.is_some() {
                    return Err(de::Error::duplicate_field("llm"));
                }
                selected = map.next_value_seed(SelectedLlmClientSectionSeed {
                    requested_id: self.requested_id,
                })?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(selected)
    }
}

struct SelectedLlmClientSectionSeed<'a> {
    requested_id: &'a str,
}

impl<'de> DeserializeSeed<'de> for SelectedLlmClientSectionSeed<'_> {
    type Value = Option<RawLlmClientConfiguration>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(SelectedLlmClientSectionVisitor {
            requested_id: self.requested_id,
        })
    }
}

struct SelectedLlmClientSectionVisitor<'a> {
    requested_id: &'a str,
}

impl<'de> Visitor<'de> for SelectedLlmClientSectionVisitor<'_> {
    type Value = Option<RawLlmClientConfiguration>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("只包含 clients 的 LLM 配置")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut selected = None;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "clients" => {
                    if selected.is_some() {
                        return Err(de::Error::duplicate_field("clients"));
                    }
                    selected = map.next_value_seed(SelectedLlmClientMapSeed {
                        requested_id: self.requested_id,
                    })?;
                }
                _ => return Err(de::Error::unknown_field(&key, &["clients"])),
            }
        }
        Ok(selected)
    }
}

struct SelectedLlmClientMapSeed<'a> {
    requested_id: &'a str,
}

impl<'de> DeserializeSeed<'de> for SelectedLlmClientMapSeed<'_> {
    type Value = Option<RawLlmClientConfiguration>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(SelectedLlmClientMapVisitor {
            requested_id: self.requested_id,
        })
    }
}

struct SelectedLlmClientMapVisitor<'a> {
    requested_id: &'a str,
}

impl<'de> Visitor<'de> for SelectedLlmClientMapVisitor<'_> {
    type Value = Option<RawLlmClientConfiguration>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("按 ID 命名的 LLM 客户端表")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut selected = None;
        while let Some(id) = map.next_key::<String>()? {
            if id == self.requested_id {
                if selected.is_some() {
                    return Err(de::Error::custom("所选 LLM 客户端 ID 重复"));
                }
                selected = Some(map.next_value::<RawLlmClientConfiguration>()?);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(selected)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInitSelection {
    #[serde(default, rename = "llm")]
    _llm: Option<IgnoredAny>,
    #[serde(default, rename = "prompts")]
    _prompts: Option<IgnoredAny>,
    #[serde(default, rename = "languages")]
    _languages: Option<IgnoredAny>,
    #[serde(default, rename = "translation")]
    _translation: Option<IgnoredAny>,
    #[serde(default, rename = "write_back")]
    _write_back: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTestSelection {
    llm: RawTestLlmConfiguration,
    #[serde(default, rename = "prompts")]
    _prompts: Option<IgnoredAny>,
    #[serde(default, rename = "languages")]
    _languages: Option<IgnoredAny>,
    #[serde(default, rename = "translation")]
    _translation: Option<IgnoredAny>,
    #[serde(default, rename = "write_back")]
    _write_back: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTestLlmConfiguration {
    clients: BTreeMap<String, RawLlmClientConfiguration>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExtractSelection {
    #[serde(default, rename = "llm")]
    _llm: Option<IgnoredAny>,
    #[serde(default, rename = "prompts")]
    _prompts: Option<IgnoredAny>,
    #[serde(default, rename = "languages")]
    _languages: Option<IgnoredAny>,
    #[serde(default, rename = "translation")]
    _translation: Option<IgnoredAny>,
    #[serde(default, rename = "write_back")]
    _write_back: Option<IgnoredAny>,
}

#[derive(Deserialize)]
struct RawLanguageDiscriminatorSelection {
    languages: Vec<RawLanguageDiscriminator>,
    #[serde(flatten)]
    _other: HashMap<String, IgnoredAny>,
}

#[derive(Deserialize)]
struct RawLanguageDiscriminator {
    #[serde(rename = "type")]
    language_type: String,
    #[serde(flatten)]
    _other: HashMap<String, IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTranslateSelection {
    prompts: RawPromptsConfiguration,
    languages: Vec<RawLanguageConfiguration>,
    translation: RawTranslationSelection,
    #[serde(default, rename = "llm")]
    _llm: Option<IgnoredAny>,
    #[serde(default, rename = "write_back")]
    _write_back: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWriteBackSelection {
    #[serde(default)]
    write_back: RawWriteBackConfiguration,
    #[serde(default, rename = "llm")]
    _llm: Option<IgnoredAny>,
    #[serde(default, rename = "prompts")]
    _prompts: Option<IgnoredAny>,
    #[serde(default, rename = "languages")]
    _languages: Option<IgnoredAny>,
    #[serde(default, rename = "translation")]
    _translation: Option<IgnoredAny>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWriteBackConfiguration {
    #[serde(default = "default_enabled")]
    repair_punctuation: bool,
    #[serde(default = "default_enabled")]
    complete_continuation_whitespace: bool,
}

impl Default for RawWriteBackConfiguration {
    fn default() -> Self {
        Self {
            repair_punctuation: true,
            complete_continuation_whitespace: true,
        }
    }
}

const fn default_enabled() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManualSelection {
    languages: Vec<RawLanguageConfiguration>,
    #[serde(default, rename = "llm")]
    _llm: Option<IgnoredAny>,
    #[serde(default, rename = "prompts")]
    _prompts: Option<IgnoredAny>,
    #[serde(default, rename = "translation")]
    _translation: Option<IgnoredAny>,
    #[serde(default, rename = "write_back")]
    _write_back: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTranslationSelection {
    #[serde(default = "default_record_translation_tasks")]
    record_translation_tasks: bool,
    #[serde(rename = "profiles")]
    _profiles: IgnoredAny,
}

const fn default_record_translation_tasks() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPromptsConfiguration {
    thinking_output: bool,
    source_echo: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawProxyConfiguration {
    Disabled(bool),
    Url(String),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum RawLanguageConfiguration {
    Japanese {
        id: String,
        minimum_kana_characters: u64,
        allowed_terms: Vec<String>,
    },
    English {
        id: String,
        minimum_word_count: u64,
        minimum_letter_count: u64,
        ignored_terms: Vec<String>,
        minimum_copied_word_count: u64,
        minimum_copied_letter_count: u64,
        allowed_terms: Vec<String>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSelectedTranslationProfileConfiguration {
    id: String,
    llm_client: String,
    target_task_user_message_characters: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLlmClientConfiguration {
    url: String,
    #[serde(default)]
    protocol: RawOpenAiProtocol,
    #[serde(deserialize_with = "deserialize_api_key")]
    api_key: SecretString,
    model: String,
    stream: bool,
    max_concurrent_requests: u64,
    connect_timeout_ms: u64,
    read_timeout_ms: u64,
    request_timeout_ms: u64,
    proxy: RawProxyConfiguration,
    additional_pem_files: Vec<PathBuf>,
    retry_delays_ms: Vec<u64>,
    max_retry_after_ms: u64,
    parameters: String,
    #[serde(default)]
    rate_limit: Option<RawLlmRateLimitConfiguration>,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawOpenAiProtocol {
    #[default]
    ChatCompletions,
    Responses,
}

impl From<RawOpenAiProtocol> for OpenAiProtocol {
    fn from(value: RawOpenAiProtocol) -> Self {
        match value {
            RawOpenAiProtocol::ChatCompletions => Self::ChatCompletions,
            RawOpenAiProtocol::Responses => Self::Responses,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLlmRateLimitConfiguration {
    requests_per_minute: u64,
    burst: u64,
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use secrecy::ExposeSecret;

    use super::*;
    use crate::application::arguments::{AttArguments, ProductCommand};
    use crate::rpg_maker::RpgMakerEngine;

    const EXAMPLE_TASK_RECORDING: &str = "record_translation_tasks = true";
    const EXAMPLE_TARGET_CHARACTERS: &str = "target_task_user_message_characters = 24000";
    const EXAMPLE_CLIENT_STREAM: &str = "stream = false";
    const EXAMPLE_CLIENT_PARAMETERS: &str = "parameters = '''\n{}\n'''";

    #[test]
    fn repository_example_is_valid_for_every_command() {
        let directory = TestDirectory::new();
        let path = directory.write("config.toml", include_str!("../../config.example.toml"));

        for command in [
            init_command(),
            extract_command(false),
            translate_command("primary"),
            write_back_command(),
            manual_export_command(),
            manual_check_command(),
            manual_apply_command(),
            project_lua_command(),
        ] {
            load_configuration(&path, command).expect("仓库示例必须满足每个命令的当前契约");
        }
    }

    #[test]
    fn llm_protocol_defaults_to_chat_completions_and_explicit_responses_changes_endpoint() {
        let directory = TestDirectory::new();
        let example = include_str!("../../config.example.toml");
        let without_protocol = example.replacen("protocol = \"chat_completions\"\n", "", 1);
        let path = directory.write("default-protocol.toml", &without_protocol);
        let ConfiguredRpgMakerCommand::Translate(configured) =
            load_configuration(&path, translate_command("primary"))
                .expect("省略协议时应使用 Chat Completions")
        else {
            panic!("应建立 Translate 配置");
        };
        assert_eq!(
            configured.client().protocol(),
            OpenAiProtocol::ChatCompletions
        );
        assert_eq!(
            configured.client().endpoint().as_str(),
            "https://api.example.com/v1/chat/completions"
        );

        let responses = example.replacen(
            "protocol = \"chat_completions\"",
            "protocol = \"responses\"",
            1,
        );
        let path = directory.write("responses-protocol.toml", &responses);
        let ConfiguredRpgMakerCommand::Translate(configured) =
            load_configuration(&path, translate_command("primary"))
                .expect("显式 Responses 协议应建立配置")
        else {
            panic!("应建立 Translate 配置");
        };
        assert_eq!(configured.client().protocol(), OpenAiProtocol::Responses);
        assert_eq!(
            configured.client().endpoint().as_str(),
            "https://api.example.com/v1/responses"
        );
    }

    #[test]
    fn llm_protocol_rejects_unknown_or_non_string_values() {
        let directory = TestDirectory::new();
        let example = include_str!("../../config.example.toml");
        for (name, replacement) in [
            ("unknown", "protocol = \"completions\""),
            ("wrong-type", "protocol = []"),
        ] {
            let source = example.replacen("protocol = \"chat_completions\"", replacement, 1);
            let path = directory.write(&format!("{name}.toml"), &source);
            assert!(
                load_configuration(&path, translate_command("primary")).is_err(),
                "无效协议值 {replacement} 必须拒绝"
            );
        }
    }

    #[test]
    fn llm_stream_accepts_both_explicit_boolean_values() {
        let directory = TestDirectory::new();
        let example = include_str!("../../config.example.toml");
        for expected in [false, true] {
            let source = example.replacen(
                EXAMPLE_CLIENT_STREAM,
                format!("stream = {expected}").as_str(),
                1,
            );
            let path = directory.write(format!("stream-{expected}.toml").as_str(), &source);
            let ConfiguredRpgMakerCommand::Translate(configured) =
                load_configuration(&path, translate_command("primary"))
                    .expect("显式流式开关应建立 Translate 配置")
            else {
                panic!("应建立 Translate 配置");
            };
            assert_eq!(configured.client().stream(), expected);
        }
    }

    #[test]
    fn llm_parameters_reserve_only_the_selected_protocol_fields() {
        let directory = TestDirectory::new();
        let example = include_str!("../../config.example.toml");
        for (name, protocol, reserved, allowed) in [
            ("chat-messages", "chat_completions", "messages", "input"),
            ("chat-stream", "chat_completions", "stream", "input"),
            ("responses-input", "responses", "input", "messages"),
            ("responses-stream", "responses", "stream", "messages"),
            (
                "responses-background",
                "responses",
                "background",
                "messages",
            ),
        ] {
            let source = example
                .replacen(
                    "protocol = \"chat_completions\"",
                    format!("protocol = \"{protocol}\"").as_str(),
                    1,
                )
                .replacen(
                    EXAMPLE_CLIENT_PARAMETERS,
                    format!("parameters = '''\n{{\"{reserved}\":[]}}\n'''").as_str(),
                    1,
                );
            let path = directory.write(&format!("{name}-reserved.toml"), &source);
            assert!(matches!(
                load_configuration(&path, translate_command("primary")),
                Err(ConfigurationLoadError::InvalidValue(_)
                    | ConfigurationLoadError::InvalidValueAtPath { .. })
            ));

            let source = example
                .replacen(
                    "protocol = \"chat_completions\"",
                    format!("protocol = \"{protocol}\"").as_str(),
                    1,
                )
                .replacen(
                    EXAMPLE_CLIENT_PARAMETERS,
                    format!("parameters = '''\n{{\"{allowed}\":[]}}\n'''").as_str(),
                    1,
                );
            let path = directory.write(&format!("{name}-allowed.toml"), &source);
            load_configuration(&path, translate_command("primary"))
                .expect("另一协议拥有的字段不应被当前协议无依据地保留");
        }
    }

    #[test]
    fn manual_check_and_apply_do_not_parse_language_configuration() {
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml").replacen(
            "minimum_kana_characters = 1",
            "minimum_kana_characters = \"invalid\"",
            1,
        );
        let path = directory.write("manual-unselected-languages.toml", &source);

        load_configuration(&path, manual_check_command()).expect("Manual check 不应解释语言配置");
        load_configuration(&path, manual_apply_command()).expect("Manual apply 不应解释语言配置");
        assert!(
            load_configuration(&path, manual_export_command()).is_err(),
            "Manual export 必须继续校验用于筛选待译项的语言配置"
        );
    }

    #[test]
    fn distribution_layout_uses_only_the_executable_directory() {
        let root = absolute_test_path("release");
        let executable = root.join("att.exe");
        let distribution = DistributionLayout::from_executable_path(executable)
            .expect("拥有父目录的可执行文件路径应建立发行布局");
        assert_eq!(distribution.configuration_path(), root.join("config.toml"));
        assert_eq!(distribution.projects_root(), root.join("projects"));
        assert_eq!(distribution.prompts_root(), root.join("prompts"));

        assert!(matches!(
            DistributionLayout::from_executable_path(PathBuf::from("att.exe")),
            Err(DistributionLayoutError::ExecutableDirectoryMissing { .. })
        ));

        let current = DistributionLayout::from_current_executable()
            .expect("测试进程必须能确定自己的可执行文件");
        let current_root = std::env::current_exe()
            .expect("测试进程路径应可读取")
            .parent()
            .expect("测试进程路径应有父目录")
            .to_path_buf();
        assert_eq!(
            current.configuration_path(),
            current_root.join("config.toml")
        );
    }

    #[test]
    fn missing_fixed_configuration_reports_the_executable_sibling_path() {
        let directory = TestDirectory::new();
        let distribution =
            DistributionLayout::from_executable_path(directory.path().join("att.exe"))
                .expect("测试发行布局应合法");
        let error = match load_product_configuration(
            &distribution,
            ProductCommand::Mz {
                command: init_command(),
            },
        ) {
            Ok(_) => panic!("缺少固定配置时必须失败"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            ConfigurationLoadError::Open { ref path, .. }
                if path == &directory.path().join("config.toml")
        ));
    }

    #[test]
    fn invalid_utf8_builds_the_current_literal_diagnostic_contract() {
        let error = ConfigurationLoadError::InvalidUtf8 {
            path: PathBuf::from("C:/ATT/config.toml"),
            valid_up_to: 7,
            error_len: Some(2),
        };
        let actual = serde_json::to_value(error.diagnostic()).expect("诊断必须可序列化");
        let expected: serde_json::Value = serde_json::from_str(
            r#"{
                "code":"configuration.invalid_utf8",
                "stage":"configuration",
                "issue":{
                    "family":"configuration",
                    "details":{
                        "kind":"invalid_utf8",
                        "path":"C:/ATT/config.toml",
                        "valid_up_to":7,
                        "error_len":2
                    }
                },
                "resolution":"fix_configuration"
            }"#,
        )
        .expect("独立字面量 JSON 必须有效");
        assert_eq!(actual, expected);
    }

    #[test]
    fn missing_distribution_directory_builds_a_typed_path_diagnostic() {
        let error = DistributionLayoutError::ExecutableDirectoryMissing {
            path: PathBuf::from("att.exe"),
        };
        assert_eq!(
            serde_json::to_value(error.diagnostic()).expect("发行布局诊断必须可序列化"),
            serde_json::json!({
                "code": "filesystem.invalid_path",
                "stage": "process_startup",
                "issue": {
                    "family": "file_system",
                    "details": {
                        "context": {
                            "stage": "process_startup",
                            "operation": "resolve_directory"
                        },
                        "problem": {
                            "kind": "invalid_path",
                            "path": "att.exe",
                            "violation": "missing_parent"
                        }
                    }
                },
                "resolution": "report_bug"
            })
        );
    }

    #[test]
    fn project_root_is_the_fixed_sibling_of_the_executable() {
        let directory = TestDirectory::new();
        let path = directory.write("config.toml", minimal_init_configuration());
        let ConfiguredRpgMakerCommand::Init(configured) =
            load_configuration(&path, init_command()).expect("最小 Init 配置应合法")
        else {
            panic!("应建立 Init 配置");
        };
        assert_eq!(
            configured.common().projects_root(),
            directory.path().join("projects")
        );
    }

    #[test]
    fn directory_publisher_lock_root_is_namespaced_by_engine() {
        let projects_root = absolute_test_path("projects");
        let configured = build_directory_publisher_configuration(
            &projects_root,
            RpgMakerEngine::Mz.storage_name(),
        );
        assert_eq!(
            configured.lock_directory(),
            projects_root.join(".att-locks/directory-publish/mz")
        );

        let configured = build_directory_publisher_configuration(
            &projects_root,
            RpgMakerEngine::Mv.storage_name(),
        );
        assert_eq!(
            configured.lock_directory(),
            projects_root.join(".att-locks/directory-publish/mv")
        );
    }

    #[test]
    fn init_does_not_parse_unselected_product_sections() {
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml")
            .replace("api_key = \"replace-with-api-key\"", "api_key = []");
        let path = directory.write("init.toml", &source);

        let configured =
            load_configuration(&path, init_command()).expect("Init 不应解析未选择的 LLM 配置");
        assert!(matches!(configured, ConfiguredRpgMakerCommand::Init(_)));
    }

    #[test]
    fn non_translate_commands_load_their_minimal_configuration() {
        let directory = TestDirectory::new();
        let path = directory.write("minimal.toml", minimal_init_configuration());

        for command in [init_command(), extract_command(false), write_back_command()] {
            load_configuration(&path, command)
                .expect("非 Translate 命令不应要求无现实消费的配置存在");
        }
    }

    #[test]
    fn write_back_text_switches_default_to_enabled_and_can_be_selected_independently() {
        let directory = TestDirectory::new();
        let default_path = directory.write("write-back-default.toml", minimal_init_configuration());
        let ConfiguredRpgMakerCommand::WriteBack(defaults) =
            load_configuration(&default_path, write_back_command())
                .expect("省略 write_back 表时必须使用正式默认")
        else {
            panic!("必须建立 WriteBack 配置")
        };
        let defaults = defaults.rpg_maker().text();
        assert!(defaults.repair_punctuation());
        assert!(defaults.complete_continuation_whitespace());

        for (name, repair_punctuation, complete_continuation_whitespace) in [
            ("punctuation-only", true, false),
            ("whitespace-only", false, true),
            ("both-disabled", false, false),
        ] {
            let file_name = format!("write-back-{name}.toml");
            let path = directory.write(
                &file_name,
                &format!(
                    "[write_back]\nrepair_punctuation = {repair_punctuation}\ncomplete_continuation_whitespace = {complete_continuation_whitespace}\n"
                ),
            );
            let ConfiguredRpgMakerCommand::WriteBack(configured) =
                load_configuration(&path, write_back_command())
                    .expect("两个 WriteBack 开关必须可以独立选择")
            else {
                panic!("必须建立 WriteBack 配置")
            };
            let configured = configured.rpg_maker().text();
            assert_eq!(configured.repair_punctuation(), repair_punctuation);
            assert_eq!(
                configured.complete_continuation_whitespace(),
                complete_continuation_whitespace
            );
        }
    }

    #[test]
    fn omitted_translate_profile_remains_deferred_until_project_state_is_known() {
        let directory = TestDirectory::new();
        let path = directory.write(
            "deferred-profile.toml",
            include_str!("../../config.example.toml"),
        );
        let ConfiguredRpgMakerCommand::Translate(configured) =
            load_configuration(&path, translate_command_without_profile())
                .expect("省略 Profile 应先完成与 Profile 无关的配置加载")
        else {
            panic!("应建立 Translate 配置");
        };
        assert_eq!(configured.resolved_profile_id(), None);

        let configured = (*configured)
            .resolve_profile("primary")
            .expect("项目状态中的现行 Profile 应可精确解析");
        assert_eq!(configured.resolved_profile_id(), Some("primary"));
        assert_eq!(configured.translation().profile().id(), "primary");
    }

    #[test]
    fn missing_project_state_profile_has_a_distinct_configuration_error() {
        let directory = TestDirectory::new();
        let path = directory.write(
            "missing-profile.toml",
            include_str!("../../config.example.toml"),
        );
        let ConfiguredRpgMakerCommand::Translate(configured) =
            load_configuration(&path, translate_command_without_profile())
                .expect("省略 Profile 应等待项目状态解析")
        else {
            panic!("应建立 Translate 配置");
        };

        let error = match (*configured).resolve_profile("removed-profile") {
            Ok(_) => panic!("已从当前配置移除的保存 Profile 必须显式失败"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            ConfigurationLoadError::TranslationProfileNotFound {
                ref profile_id,
                ..
            } if profile_id == "removed-profile"
        ));
    }

    #[test]
    fn explicit_translate_profile_is_still_validated_during_load() {
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml").replace(
            EXAMPLE_TARGET_CHARACTERS,
            "target_task_user_message_characters = 0",
        );
        let path = directory.write("invalid-explicit-profile.toml", &source);

        let error = match load_configuration(&path, translate_command("primary")) {
            Ok(_) => panic!("显式 Profile 的无效字段必须在配置加载阶段被拒绝"),
            Err(error) => error,
        };
        let ConfigurationLoadError::InvalidValueAtPath { source: error, .. } = error else {
            panic!("user message 字符装箱目标为零时必须返回配置值错误");
        };
        assert_eq!(
            error.field(),
            "translation.profiles.target_task_user_message_characters"
        );
    }

    fn configuration_with_unselected_profile_sentinel(sentinel: &str) -> String {
        format!(
            r#"{}
[llm.clients.unused]
url = []
protocol = []
api_key = []
model = []
stream = []
max_concurrent_requests = []
connect_timeout_ms = []
read_timeout_ms = []
request_timeout_ms = []
proxy = []
additional_pem_files = []
retry_delays_ms = []
max_retry_after_ms = []
parameters = []

[[translation.profiles]]
llm_client = ["{sentinel}"]
target_task_user_message_characters = {{ marker = "{sentinel}" }}
id = "unused"
"#,
            include_str!("../../config.example.toml")
        )
    }

    #[test]
    fn translate_streams_past_unselected_client_and_profile_without_materializing_values() {
        const SENTINEL: &str = "UNSELECTED_PROFILE_DATA_SENTINEL";
        let directory = TestDirectory::new();
        let source = configuration_with_unselected_profile_sentinel(SENTINEL);
        let path = directory.write("translate.toml", &source);
        let ConfiguredRpgMakerCommand::Translate(configured) =
            load_configuration(&path, translate_command("primary"))
                .expect("无关客户端和 Profile 不应阻止本次翻译")
        else {
            panic!("应建立 Translate 配置");
        };
        configured
            .translation()
            .language_modules()
            .resolve(&LanguageId::parse("ja").expect("测试语言应合法"))
            .expect("应建立日语模块");
        configured
            .translation()
            .language_modules()
            .resolve(&LanguageId::parse("en").expect("测试语言应合法"))
            .expect("应建立英语模块");
        assert_eq!(configured.client().model(), "replace-with-model-id");
        assert_eq!(
            configured.client().api_key().expose_secret(),
            "replace-with-api-key"
        );
        let profile_debug = format!("{:?}", configured.translation().profile());
        assert!(profile_debug.contains("primary"));
        assert!(!profile_debug.contains(SENTINEL));
    }

    #[test]
    fn unselected_profile_data_never_enters_configuration_diagnostics() {
        const SENTINEL: &str = "UNSELECTED_PROFILE_DIAGNOSTIC_SENTINEL";
        let directory = TestDirectory::new();
        let source = configuration_with_unselected_profile_sentinel(SENTINEL).replacen(
            "minimum_kana_characters = 1",
            "minimum_kana_characters = 0",
            1,
        );
        let path = directory.write("invalid-language-with-unselected-profile.toml", &source);
        let error = match load_configuration(&path, translate_command("primary")) {
            Ok(_) => panic!("无效语言策略必须拒绝"),
            Err(error) => error,
        };

        let mut diagnostics = format!("{error:?}\n{error}");
        let mut source = error.source();
        while let Some(error) = source {
            diagnostics.push_str(format!("\n{error:?}\n{error}").as_str());
            source = error.source();
        }
        assert!(!diagnostics.contains(SENTINEL));
    }

    #[test]
    fn translate_validates_the_complete_global_language_catalog() {
        let directory = TestDirectory::new();
        let source = format!(
            "{}\n[[languages]]\ntype = \"english\"\nid = \"fr\"\nminimum_word_count = \"invalid\"\n",
            include_str!("../../config.example.toml")
        );
        let path = directory.write("invalid-language.toml", &source);
        assert!(
            load_configuration(&path, translate_command("primary")).is_err(),
            "Translate 必须在执行前验证全部语言条目"
        );
    }

    #[test]
    fn prompt_root_is_the_fixed_sibling_of_the_executable() {
        let directory = TestDirectory::new();
        let path = directory.write("config.toml", include_str!("../../config.example.toml"));
        let ConfiguredRpgMakerCommand::Translate(configured) =
            load_configuration(&path, translate_command("primary"))
                .expect("示例 Translate 配置应合法")
        else {
            panic!("应建立 Translate 配置");
        };
        let expected = path
            .parent()
            .expect("测试配置路径应有父目录")
            .join("prompts");
        assert_eq!(configured.translation().prompt_root(), expected);
    }

    #[test]
    fn additional_pem_paths_are_relative_to_the_executable_directory() {
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml").replace(
            "additional_pem_files = []",
            "additional_pem_files = [\"certificates/local.pem\"]",
        );
        let path = directory.write("config.toml", &source);
        let ConfiguredRpgMakerCommand::Translate(configured) =
            load_configuration(&path, translate_command("primary"))
                .expect("相对 PEM 路径应建立 Translate 配置")
        else {
            panic!("应建立 Translate 配置");
        };
        assert_eq!(
            configured.llm().additional_pem_files(),
            &[directory.path().join("certificates/local.pem")]
        );
    }

    #[test]
    fn prompt_output_switches_are_independent_required_booleans() {
        let directory = TestDirectory::new();
        for (thinking_output, source_echo) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            let source = replace_thinking_output(
                include_str!("../../config.example.toml"),
                format!("thinking_output = {thinking_output}").as_str(),
            );
            let source =
                replace_source_echo(&source, format!("source_echo = {source_echo}").as_str());
            let path = directory.write(
                format!("prompt-{thinking_output}-{source_echo}.toml").as_str(),
                &source,
            );
            let ConfiguredRpgMakerCommand::Translate(configured) =
                load_configuration(&path, translate_command("primary"))
                    .expect("两个 Prompt 输出开关的任意组合都应建立受信配置")
            else {
                panic!("应建立 Translate 配置");
            };

            assert_eq!(configured.translation().thinking_output(), thinking_output);
            assert_eq!(configured.translation().source_echo(), source_echo);
        }
    }

    #[test]
    fn commands_other_than_translate_do_not_consume_prompt_values() {
        let directory = TestDirectory::new();
        let source = replace_thinking_output(
            include_str!("../../config.example.toml"),
            "thinking_output = []",
        );
        let source = replace_source_echo(&source, "source_echo = []");
        let path = directory.write("unselected-prompts.toml", &source);

        for command in [init_command(), extract_command(false), write_back_command()] {
            load_configuration(&path, command)
                .expect("非 Translate 命令不得物化或校验 prompts 的字段值");
        }
    }

    #[test]
    fn translate_defaults_and_preserves_task_recording_selection() {
        let directory = TestDirectory::new();
        let example = include_str!("../../config.example.toml");
        let cases = [
            ("omitted", example.replace(EXAMPLE_TASK_RECORDING, ""), true),
            (
                "false",
                example.replace(EXAMPLE_TASK_RECORDING, "record_translation_tasks = false"),
                false,
            ),
            ("true", example.to_owned(), true),
        ];

        for (name, source, expected) in cases {
            let path = directory.write(
                format!("record-translation-tasks-{name}.toml").as_str(),
                &source,
            );
            let ConfiguredRpgMakerCommand::Translate(configured) =
                load_configuration(&path, translate_command("primary"))
                    .expect("任务记录开关应建立受信 Translate 配置")
            else {
                panic!("应建立 Translate 配置");
            };

            assert_eq!(configured.record_translation_tasks(), expected);
        }
    }

    #[test]
    fn only_translate_consumes_the_task_recording_value() {
        const SENTINEL: &str = "RECORD_TRANSLATION_TASKS_TYPE_SENTINEL";
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml").replace(
            EXAMPLE_TASK_RECORDING,
            format!("record_translation_tasks = [\"{SENTINEL}\"]").as_str(),
        );
        let path = directory.write("record-translation-tasks-type.toml", &source);

        for command in [init_command(), extract_command(false), write_back_command()] {
            load_configuration(&path, command)
                .expect("非 Translate 命令不得物化或校验任务记录开关");
        }

        let error = match load_configuration(&path, translate_command("primary")) {
            Ok(_) => panic!("Translate 必须拒绝非布尔任务记录开关"),
            Err(error) => error,
        };
        let diagnostics = format!("{error:?}\n{error}");
        assert!(diagnostics.contains("translation.record_translation_tasks"));
        assert!(!diagnostics.contains(SENTINEL));
    }

    #[test]
    fn selected_profile_rejects_unknown_fields() {
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml").replace(
            EXAMPLE_TARGET_CHARACTERS,
            format!("{EXAMPLE_TARGET_CHARACTERS}\nunexpected_field = []").as_str(),
        );
        let path = directory.write("unknown-profile-field.toml", &source);
        assert!(
            load_configuration(&path, translate_command("primary")).is_err(),
            "所选 Profile 必须严格拒绝未知字段"
        );
    }

    #[test]
    fn translate_rejects_every_missing_consumed_configuration_field() {
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml").replace("\r\n", "\n");
        let cases = [
            (
                "prompts-thinking-output",
                replace_thinking_output(&source, ""),
            ),
            ("prompts-source-echo", replace_source_echo(&source, "")),
            (
                "languages",
                remove_configuration_range(&source, "[[languages]]", "[[translation.profiles]]"),
            ),
            ("profile-id", source.replacen("id = \"primary\"\n", "", 1)),
            (
                "profile-client",
                source.replacen("llm_client = \"primary\"\n", "", 1),
            ),
            (
                "profile-target-task-user-message-characters",
                source.replacen(format!("{EXAMPLE_TARGET_CHARACTERS}\n").as_str(), "", 1),
            ),
            (
                "client-retry-delays",
                source.replacen("retry_delays_ms = []\n", "", 1),
            ),
            (
                "client-stream",
                source.replacen(format!("{EXAMPLE_CLIENT_STREAM}\n").as_str(), "", 1),
            ),
            (
                "client-max-retry-after",
                source.replacen("max_retry_after_ms = 1000\n", "", 1),
            ),
        ];

        for (name, source) in cases {
            let path = directory.write(format!("missing-{name}.toml").as_str(), &source);
            assert!(
                load_configuration(&path, translate_command("primary")).is_err(),
                "Translate 消费的必填项 {name} 缺失时必须显式失败"
            );
        }
    }

    #[test]
    fn toml_diagnostics_identify_missing_unknown_and_mistyped_fields_without_values() {
        let directory = TestDirectory::new();
        let source = include_str!("../../config.example.toml").replace("\r\n", "\n");
        let cases = [
            (
                "selected-profile-missing",
                source.replacen("llm_client = \"primary\"\n", "", 1),
                "translation.profiles.llm_client",
                "缺少必填字段",
                ConfigurationTomlFailureKind::MissingField,
                None,
            ),
            (
                "client-stream-missing",
                source.replacen(format!("{EXAMPLE_CLIENT_STREAM}\n").as_str(), "", 1),
                "llm.clients.primary.stream",
                "缺少必填字段",
                ConfigurationTomlFailureKind::MissingField,
                None,
            ),
            (
                "removed-projects",
                format!("{source}\n[projects]\nroot = \"REMOVED_PROJECT_ROOT_SENTINEL\"\n"),
                "projects",
                "当前配置契约不接受该字段",
                ConfigurationTomlFailureKind::UnknownField,
                Some("REMOVED_PROJECT_ROOT_SENTINEL"),
            ),
            (
                "removed-prompt-root",
                source.replacen(
                    "[prompts]\n",
                    "[prompts]\nroot = \"PROMPT_ROOT_SENTINEL\"\n",
                    1,
                ),
                "prompts.root",
                "当前配置契约不接受该字段",
                ConfigurationTomlFailureKind::UnknownField,
                Some("PROMPT_ROOT_SENTINEL"),
            ),
            (
                "unknown",
                source.replacen(
                    "[prompts]\n",
                    "[prompts]\nunexpected_prompt_field = \"UNKNOWN_VALUE_SENTINEL\"\n",
                    1,
                ),
                "prompts.unexpected_prompt_field",
                "当前配置契约不接受该字段",
                ConfigurationTomlFailureKind::UnknownField,
                Some("UNKNOWN_VALUE_SENTINEL"),
            ),
            (
                "prompt-thinking-output-type",
                replace_thinking_output(
                    &source,
                    "thinking_output = [\"PROMPT_THINKING_TYPE_SENTINEL\"]",
                ),
                "prompts.thinking_output",
                "字段类型不符合当前配置契约",
                ConfigurationTomlFailureKind::TypeMismatch {
                    expected: ConfigurationTomlValueKind::Boolean,
                },
                Some("PROMPT_THINKING_TYPE_SENTINEL"),
            ),
            (
                "prompt-source-echo-type",
                replace_source_echo(
                    &source,
                    "source_echo = [\"PROMPT_SOURCE_ECHO_TYPE_SENTINEL\"]",
                ),
                "prompts.source_echo",
                "字段类型不符合当前配置契约",
                ConfigurationTomlFailureKind::TypeMismatch {
                    expected: ConfigurationTomlValueKind::Boolean,
                },
                Some("PROMPT_SOURCE_ECHO_TYPE_SENTINEL"),
            ),
            (
                "type",
                source.replacen(
                    "max_concurrent_requests = 16",
                    "max_concurrent_requests = \"TYPE_VALUE_SENTINEL\"",
                    1,
                ),
                "llm.clients.primary.max_concurrent_requests",
                "字段类型不符合当前配置契约",
                ConfigurationTomlFailureKind::TypeMismatch {
                    expected: ConfigurationTomlValueKind::Integer,
                },
                Some("TYPE_VALUE_SENTINEL"),
            ),
            (
                "stream-type",
                source.replacen(
                    EXAMPLE_CLIENT_STREAM,
                    "stream = [\"STREAM_TYPE_SENTINEL\"]",
                    1,
                ),
                "llm.clients.primary.stream",
                "字段类型不符合当前配置契约",
                ConfigurationTomlFailureKind::TypeMismatch {
                    expected: ConfigurationTomlValueKind::Boolean,
                },
                Some("STREAM_TYPE_SENTINEL"),
            ),
            (
                "api-key-type",
                source.replacen(
                    "api_key = \"replace-with-api-key\"",
                    "api_key = [\"API_KEY_TYPE_SENTINEL\"]",
                    1,
                ),
                "llm.clients.primary.api_key",
                "字段类型不符合当前配置契约",
                ConfigurationTomlFailureKind::TypeMismatch {
                    expected: ConfigurationTomlValueKind::String,
                },
                Some("API_KEY_TYPE_SENTINEL"),
            ),
        ];

        for (name, source, expected_resource, expected_reason, expected_failure, forbidden_value) in
            cases
        {
            let path = directory.write(format!("diagnostic-{name}.toml").as_str(), &source);
            let error = match load_configuration(&path, translate_command("primary")) {
                Ok(_) => panic!("无效配置必须失败"),
                Err(error) => error,
            };
            let diagnostic = error.to_string();
            let ConfigurationLoadError::InvalidToml {
                path: error_path,
                location,
                resource,
                failure,
            } = &error
            else {
                panic!("结构、字段和类型失败必须保留 typed TOML 诊断：{error:?}");
            };

            assert!(
                diagnostic.starts_with(path.display().to_string().as_str()),
                "诊断必须以配置路径开始：{diagnostic}"
            );
            assert_eq!(error_path, &path);
            assert!(location.is_some(), "已定位的配置失败必须携带一基行列");
            assert_eq!(resource, expected_resource);
            assert_eq!(*failure, expected_failure);
            assert!(diagnostic.contains(expected_resource), "{diagnostic}");
            assert!(diagnostic.contains(expected_reason), "{diagnostic}");
            if let Some(value) = forbidden_value {
                assert!(
                    !diagnostic.contains(value),
                    "诊断不得回显原始值：{diagnostic}"
                );
                assert!(
                    !format!("{error:?}").contains(value),
                    "Debug 诊断不得回显原始值"
                );
            }
        }
    }

    #[test]
    fn translate_rejects_duplicate_current_profile_and_language_ids() {
        let directory = TestDirectory::new();
        let duplicate_profile = format!(
            "{}\n[[translation.profiles]]\nid = \"primary\"\n",
            include_str!("../../config.example.toml")
        );
        let path = directory.write("duplicate-profile.toml", &duplicate_profile);
        assert!(load_configuration(&path, translate_command("primary")).is_err());

        let duplicate_language = format!(
            "{}\n[[languages]]\ntype = \"japanese\"\nid = \"JA\"\nminimum_kana_characters = 1\nallowed_terms = []\n",
            include_str!("../../config.example.toml")
        );
        let path = directory.write("duplicate-language.toml", &duplicate_language);
        assert!(load_configuration(&path, translate_command("primary")).is_err());
    }

    #[test]
    fn selected_subtrees_remain_strict() {
        let directory = TestDirectory::new();
        let invalid_client = include_str!("../../config.example.toml")
            .replace("model = \"replace-with-model-id\"", "model = []");
        let path = directory.write("client.toml", &invalid_client);
        assert!(load_configuration(&path, translate_command("primary")).is_err());
    }

    #[test]
    fn unknown_fields_are_rejected_across_selected_and_unselected_sections() {
        let directory = TestDirectory::new();
        let example = include_str!("../../config.example.toml");
        let cases = [
            ("top", format!("{example}\n[unknown]\nvalue = 1\n")),
            (
                "removed-projects",
                format!("{example}\n[projects]\nroot = \"REMOVED_PROJECT_ROOT_SENTINEL\"\n"),
            ),
            (
                "prompts",
                replace_thinking_output(example, "thinking_output = false\nunexpected = 1"),
            ),
            (
                "llm",
                example.replace(
                    "[llm.clients.primary]",
                    "[llm]\nunexpected = 1\n\n[llm.clients.primary]",
                ),
            ),
            (
                "client",
                example.replace(
                    "model = \"replace-with-model-id\"",
                    "model = \"replace-with-model-id\"\nunexpected = 1",
                ),
            ),
            (
                "rate-limit",
                format!(
                    "{example}\n[llm.clients.primary.rate_limit]\nrequests_per_minute = 60\nburst = 8\nunexpected = 1\n"
                ),
            ),
            (
                "language",
                example.replacen(
                    "allowed_terms = []",
                    "allowed_terms = []\nunexpected = 1",
                    1,
                ),
            ),
            (
                "profile",
                example.replace(
                    EXAMPLE_TARGET_CHARACTERS,
                    format!("{EXAMPLE_TARGET_CHARACTERS}\nunexpected = 1").as_str(),
                ),
            ),
            (
                "rpg-maker",
                example.replace(
                    EXAMPLE_TASK_RECORDING,
                    format!("{EXAMPLE_TASK_RECORDING}\nunexpected = 1").as_str(),
                ),
            ),
        ];

        for (name, source) in cases {
            let path = directory.write(format!("unknown-{name}.toml").as_str(), &source);
            assert!(
                load_configuration(&path, init_command()).is_err(),
                "Init 未选择的分区也必须拒绝未知字段：{name}"
            );
        }
    }

    #[test]
    fn duplicate_toml_keys_are_rejected_even_inside_unselected_sections() {
        let directory = TestDirectory::new();
        let source = format!(
            "{}\n[llm.clients.unused]\napi_key = \"first\"\napi_key = \"second\"\n",
            minimal_init_configuration()
        );
        let path = directory.write("duplicate-unselected-key.toml", &source);

        let error = match load_configuration(&path, init_command()) {
            Ok(_) => panic!("完整 TOML 的重复键不得因分区未选中而忽略"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            ConfigurationLoadError::InvalidToml {
                ref resource,
                failure: ConfigurationTomlFailureKind::DuplicateField,
                ..
            } if resource == "llm.clients.unused.api_key"
        ));
    }

    #[test]
    fn selected_scalar_shape_conflicts_are_typed_without_value_echo() {
        const SENTINEL: &str = "SCALAR_SHAPE_VALUE_SENTINEL";
        let directory = TestDirectory::new();
        let example = include_str!("../../config.example.toml").replace("\r\n", "\n");
        assert!(example.contains(EXAMPLE_CLIENT_PARAMETERS));

        for (name, replacement) in [
            (
                "table-header",
                format!("[llm.clients.primary.parameters]\nvendor = \"{SENTINEL}\""),
            ),
            ("dotted-key", format!("parameters.vendor = \"{SENTINEL}\"")),
            (
                "inline-table",
                format!("parameters = {{ vendor = \"{SENTINEL}\" }}"),
            ),
        ] {
            let source = example.replacen(EXAMPLE_CLIENT_PARAMETERS, &replacement, 1);
            let path = directory.write(format!("scalar-shape-{name}.toml").as_str(), &source);
            let error = match load_configuration(&path, translate_command("primary")) {
                Ok(_) => panic!("所选字符串字段的表形态必须拒绝：{name}"),
                Err(error) => error,
            };
            assert!(matches!(
                &error,
                ConfigurationLoadError::InvalidToml {
                    resource,
                    failure: ConfigurationTomlFailureKind::TypeMismatch {
                        expected: ConfigurationTomlValueKind::String,
                    },
                    ..
                } if resource == "llm.clients.primary.parameters"
            ));
            assert!(
                !format!("{error:?}\n{error}").contains(SENTINEL),
                "shape 诊断不得回显字段正文：{name}"
            );
        }
    }

    #[test]
    fn removed_quote_repair_configuration_is_rejected() {
        let directory = TestDirectory::new();
        let example = include_str!("../../config.example.toml");
        let source = example.replacen(
            "allowed_terms = []",
            "allowed_terms = []\nquote_repair_pairs = [[\"“\", \"”\"]]",
            1,
        );
        let path = directory.write("removed-quote-repair.toml", &source);
        let error = match load_configuration(&path, translate_command("primary")) {
            Ok(_) => panic!("旧的日文引号修复字段必须按未知字段拒绝"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            ConfigurationLoadError::InvalidToml {
                ref resource,
                failure: ConfigurationTomlFailureKind::UnknownField,
                ..
            } if resource == "languages.quote_repair_pairs"
        ));
    }

    #[test]
    fn toml_value_table_structure_conflicts_are_duplicate_fields() {
        let directory = TestDirectory::new();
        for (name, conflict) in [
            ("value-then-table", "a = 1\n[a]\n"),
            ("table-value-then-table", "[a]\nb = 1\n[a.b]\n"),
            (
                "inline-value-then-dotted",
                "a = { value = 1, value.child = 2 }\n",
            ),
            ("value-then-dotted", "a = 1\na.child = 2\n"),
        ] {
            let source = format!("{conflict}\n{}", minimal_init_configuration());
            let path =
                directory.write(format!("duplicate-structure-{name}.toml").as_str(), &source);
            let error = match load_configuration(&path, init_command()) {
                Ok(_) => panic!("标量与表结构占用冲突必须拒绝：{name}"),
                Err(error) => error,
            };
            assert!(
                matches!(
                    error,
                    ConfigurationLoadError::InvalidToml {
                        failure: ConfigurationTomlFailureKind::DuplicateField,
                        ..
                    }
                ),
                "结构占用冲突必须稳定分类为 DuplicateField：{name}"
            );
        }
    }

    #[test]
    fn syntax_and_discriminator_value_failures_are_typed_without_value_echo() {
        let directory = TestDirectory::new();
        let syntax_path = directory.write(
            "syntax.toml",
            "[prompts]\nthinking_output = \"UNTERMINATED_VALUE_SENTINEL\n",
        );
        let syntax = match load_configuration(&syntax_path, init_command()) {
            Ok(_) => panic!("无效 TOML 语法必须拒绝"),
            Err(error) => error,
        };
        assert!(matches!(
            &syntax,
            ConfigurationLoadError::InvalidToml {
                failure: ConfigurationTomlFailureKind::Syntax,
                ..
            }
        ));
        assert!(!format!("{syntax:?}\n{syntax}").contains("UNTERMINATED_VALUE_SENTINEL"));

        const INVALID_TYPE: &str = "INVALID_LANGUAGE_TYPE_VALUE_SENTINEL";
        let source = include_str!("../../config.example.toml").replacen(
            "type = \"japanese\"",
            format!("type = \"{INVALID_TYPE}\"").as_str(),
            1,
        );
        let value_path = directory.write("language-type.toml", &source);
        let value = match load_configuration(&value_path, translate_command("primary")) {
            Ok(_) => panic!("未知语言类型必须拒绝"),
            Err(error) => error,
        };
        assert!(matches!(
            &value,
            ConfigurationLoadError::InvalidToml {
                resource,
                failure: ConfigurationTomlFailureKind::InvalidValue,
                ..
            } if resource == "languages.type"
        ));
        assert!(!format!("{value:?}\n{value}").contains(INVALID_TYPE));
    }

    #[test]
    fn syntax_failure_after_value_uses_safe_current_field_path() {
        const API_KEY: &str = "MALFORMED_API_KEY_UNIT_SENTINEL";
        let directory = TestDirectory::new();
        let source = format!(
            r#"[llm.clients.invalid-api-key]
url = "https://example.invalid/v1/chat/completions"
api_key = "{API_KEY}" "invalid"
"#
        );
        let path = directory.write("malformed-api-key.toml", &source);

        let error = match load_configuration(&path, init_command()) {
            Ok(_) => panic!("值后出现同一行尾随内容时必须拒绝"),
            Err(error) => error,
        };
        assert!(matches!(
            &error,
            ConfigurationLoadError::InvalidToml {
                resource,
                failure: ConfigurationTomlFailureKind::Syntax,
                ..
            } if resource == "llm.clients.invalid-api-key.api_key"
        ));
        assert!(
            !format!("{error:?}\n{error}").contains(API_KEY),
            "语法诊断不得保存或回显值正文"
        );
    }

    #[test]
    fn selected_llm_client_debug_hides_only_api_key() {
        let directory = TestDirectory::new();
        let example = include_str!("../../config.example.toml").replace("\r\n", "\n");
        const CUSTOM_PARAMETERS: &str =
            "parameters = '''\n{\"vendor_value\":\"PARAMETER_SENTINEL\"}\n'''";
        assert!(
            example.contains(EXAMPLE_CLIENT_PARAMETERS),
            "示例配置必须保留本测试声明的当前 parameters"
        );
        let source = example
            .replace("replace-with-api-key", "API_KEY_SENTINEL")
            .replace(EXAMPLE_CLIENT_PARAMETERS, CUSTOM_PARAMETERS);
        assert!(source.contains("PARAMETER_SENTINEL"));
        let path = directory.write("api-key.toml", &source);
        let ConfiguredRpgMakerCommand::Translate(configured) =
            load_configuration(&path, translate_command("primary")).expect("所选客户端应合法")
        else {
            panic!("应建立 Translate 配置");
        };
        let debug = format!("{:?}", configured.client());
        assert!(!debug.contains("API_KEY_SENTINEL"));
        assert!(debug.contains("PARAMETER_SENTINEL"));
    }

    #[test]
    fn unselected_client_api_key_never_enters_configuration_diagnostics() {
        let directory = TestDirectory::new();
        let source = format!(
            "{}\n[llm.clients.unused]\nurl = []\nprotocol = []\napi_key = \"UNSELECTED_API_KEY_SENTINEL\"\nmodel = []\nstream = []\nmax_concurrent_requests = []\nconnect_timeout_ms = []\nread_timeout_ms = []\nrequest_timeout_ms = []\nproxy = []\nadditional_pem_files = []\nretry_delays_ms = []\nmax_retry_after_ms = []\nparameters = []\n",
            include_str!("../../config.example.toml")
        );
        let source = replace_thinking_output(&source, "thinking_output = []");
        let path = directory.write("unselected-api-key.toml", &source);
        let error = match load_configuration(&path, translate_command("primary")) {
            Ok(_) => panic!("无效 Prompt 配置必须拒绝"),
            Err(error) => error,
        };
        let mut diagnostics = format!("{error:?}\n{error}");
        let mut source = error.source();
        while let Some(error) = source {
            diagnostics.push_str(format!("\n{error:?}\n{error}").as_str());
            source = error.source();
        }
        assert!(!diagnostics.contains("UNSELECTED_API_KEY_SENTINEL"));
    }

    #[cfg(feature = "release-stress")]
    #[test]
    fn release_stress_large_configuration_file_is_loaded_without_an_att_size_limit() {
        let directory = TestDirectory::new();
        let path = directory.path().join("large.toml");
        let mut source = minimal_init_configuration().to_owned();
        source.push_str("\n#");
        source.push_str(&"x".repeat(5 * 1024 * 1024));
        fs::write(&path, source).expect("应写入大配置");
        load_configuration(&path, init_command()).expect("配置大小不得触发 ATT 自行规定的拒绝");
    }

    fn init_command() -> MzCommand {
        parse_command(["att", "mz", "init", "--name", "demo", "--path", "game"])
    }

    fn extract_command(builtin: bool) -> MzCommand {
        if builtin {
            parse_command(["att", "mz", "extract", "--name", "demo", "--builtin"])
        } else {
            parse_command([
                "att",
                "mz",
                "extract",
                "--name",
                "demo",
                "--rules",
                "rules.toml",
            ])
        }
    }

    fn translate_command(profile: &str) -> MzCommand {
        parse_command(["att", "mz", "translate", "--name", "demo", profile])
    }

    fn translate_command_without_profile() -> MzCommand {
        parse_command(["att", "mz", "translate", "--name", "demo"])
    }

    fn write_back_command() -> MzCommand {
        parse_command(["att", "mz", "write-back", "--name", "demo"])
    }

    fn manual_export_command() -> MzCommand {
        parse_command([
            "att",
            "mz",
            "manual",
            "export",
            "--name",
            "demo",
            "manual.toml",
        ])
    }

    fn manual_check_command() -> MzCommand {
        parse_command([
            "att",
            "mz",
            "manual",
            "check",
            "--name",
            "demo",
            "manual.toml",
        ])
    }

    fn manual_apply_command() -> MzCommand {
        parse_command([
            "att",
            "mz",
            "manual",
            "apply",
            "--name",
            "demo",
            "manual.toml",
        ])
    }

    fn project_lua_command() -> MzCommand {
        parse_command(["att", "mz", "lua", "--name", "demo", "script.lua"])
    }

    fn parse_command<const N: usize>(arguments: [&str; N]) -> MzCommand {
        parse_command_vec(arguments.into_iter().collect())
    }

    fn parse_command_vec(arguments: Vec<&str>) -> MzCommand {
        let parsed = AttArguments::try_parse_from(arguments).expect("测试命令应合法");
        match parsed.product {
            ProductCommand::Mz { command } => command,
            ProductCommand::Test => panic!("配置测试只应构造 MZ 命令"),
            ProductCommand::Mv { .. } => panic!("配置测试只应构造 MZ 命令"),
            ProductCommand::Generic { .. } => panic!("配置测试只应构造 MZ 命令"),
        }
    }

    fn absolute_test_path(label: &str) -> PathBuf {
        std::env::temp_dir().join("att-config-tests").join(label)
    }

    fn minimal_init_configuration() -> &'static str {
        ""
    }

    fn remove_configuration_range(source: &str, start: &str, end: &str) -> String {
        let start_offset = source.find(start).expect("测试配置应包含起始标记");
        let relative_end = source[start_offset..]
            .find(end)
            .expect("测试配置应包含结束标记");
        let end_offset = start_offset + relative_end;
        format!("{}{}", &source[..start_offset], &source[end_offset..])
    }

    fn replace_thinking_output(source: &str, replacement: &str) -> String {
        for current in ["thinking_output = false", "thinking_output = true"] {
            if source.contains(current) {
                return source.replacen(current, replacement, 1);
            }
        }
        panic!("测试配置应包含 thinking_output 布尔字段");
    }

    fn replace_source_echo(source: &str, replacement: &str) -> String {
        for current in ["source_echo = false", "source_echo = true"] {
            if source.contains(current) {
                return source.replacen(current, replacement, 1);
            }
        }
        panic!("测试配置应包含 source_echo 布尔字段");
    }

    struct TestDirectory(tempfile::TempDir);

    impl TestDirectory {
        fn new() -> Self {
            Self(tempfile::tempdir().expect("应创建测试目录"))
        }

        fn path(&self) -> &Path {
            self.0.path()
        }

        fn write(&self, name: &str, content: &str) -> PathBuf {
            let path = self.0.path().join(name);
            fs::write(&path, content).expect("应写入测试配置");
            path
        }
    }
}
