//!
//! # CLI for Streaming Controller
//!
//! Command line interface to provision SC id and bind-to server/port.
//! Parameters are overwritten in the following sequence:
//!     1) default values
//!     2) custom configuration if provided, or default configuration (if not)
//!     3) cli parameters
//!

use std::path::Path;
use std::process;
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::convert::TryFrom;

use tracing::info;
use tracing::debug;
use clap::Parser;

use k8_client::K8Config;
use fluvio_types::print_cli_err;
use fluvio_types::defaults::TLS_SERVER_SECRET_NAME;
use fluvio_future::openssl::TlsAcceptor;
use fluvio_future::openssl::SslVerifyMode;

use crate::services::auth::basic::BasicRbacPolicy;
use crate::error::ScError;
use crate::config::ScConfig;

type Config = (ScConfig, Option<BasicRbacPolicy>);

/// cli options
#[derive(Debug, Parser, Default)]
#[command(name = "sc-server", about = "Streaming Controller")]
pub struct ScOpt {
    /// run in local mode
    #[arg(long, conflicts_with_all = &["k8", "read_only"], value_name = "metadata path")]
    local: Option<PathBuf>,

    /// run on k8
    #[arg(long)]
    k8: bool,

    #[arg(long)]
    /// Address for external service
    bind_public: Option<String>,

    #[arg(long)]
    /// Address for internal service
    bind_private: Option<String>,

    // k8 namespace
    #[arg(short = 'n', long = "namespace", value_name = "namespace")]
    namespace: Option<String>,

    #[clap(flatten)]
    tls: TlsConfig,

    #[arg(
        long = "authorization-scopes",
        value_name = "authorization scopes path",
        env
    )]
    x509_auth_scopes: Option<PathBuf>,

    #[arg(
        long = "authorization-policy",
        value_name = "authorization policy path",
        env
    )]
    auth_policy: Option<PathBuf>,

    /// only allow white list of controllers
    #[arg(long)]
    white_list: Vec<String>,

    /// run SC in read only mode
    #[arg(long, hide = true, conflicts_with_all = &["auth_policy"])]
    read_only: Option<PathBuf>,
}

#[derive(Debug)]
pub enum RunMode<'a> {
    Local(&'a Path),
    ReadOnly(&'a Path),
    K8s,
}

impl ScOpt {
    pub fn mode(&self) -> RunMode<'_> {
        match (&self.local, &self.read_only, self.k8) {
            (Some(metadata), _, _) => RunMode::Local(metadata),
            (_, Some(path), _) => RunMode::ReadOnly(path),
            _ => RunMode::K8s,
        }
    }

    #[allow(clippy::type_complexity)]
    fn get_sc_and_k8_config(
        mut self,
    ) -> Result<(Config, K8Config, Option<(String, TlsConfig)>), ScError> {
        let k8_config = K8Config::load().expect("no k8 config founded");
        info!(?k8_config, "k8 config");

        // if name space is specified, use one from k8 config
        if self.namespace.is_none() {
            let k8_namespace = k8_config.namespace().to_owned();
            info!("using {} as namespace from kubernetes config", k8_namespace);
            self.namespace = Some(k8_namespace);
        }

        let (sc_config, tls_option) = self.as_sc_config()?;

        Ok((sc_config, k8_config, tls_option))
    }

    /// as sc configuration, 2nd part of tls configuration(proxy addr, tls config)
    /// 3rd part is path to read only metadata config
    #[allow(clippy::wrong_self_convention)]
    fn as_sc_config(self) -> Result<(Config, Option<(String, TlsConfig)>), IoError> {
        let mut config = ScConfig::default();

        // apply our option
        if let Some(public_addr) = self.bind_public {
            config.public_endpoint = public_addr;
        }

        if let Some(private_addr) = self.bind_private {
            config.private_endpoint = private_addr;
        }

        config.x509_auth_scopes = self.x509_auth_scopes;
        config.white_list = self.white_list.into_iter().collect();
        config.read_only_metadata = self.read_only.is_some();

        // Set Configuration Authorization Policy

        let policy = match self.auth_policy {
            // Lookup a policy from a path
            Some(p) => Some(BasicRbacPolicy::try_from(p)?),
            // Use root-only default policy if no policy path is found;
            None => None,
        };

        let mut tls = self.tls;

        // if tls is on, we need to assign public service(internal) to another port
        // because public is used by proxy which forward traffic to internal public port
        if tls.tls {
            let proxy_addr = config.public_endpoint.clone();
            debug!(proxy_addr, "tls proxy addr");
            config.public_endpoint = tls.bind_non_tls_public.clone().ok_or_else(|| {
                IoError::new(
                    ErrorKind::NotFound,
                    "non tls addr for public must be specified",
                )
            })?;
            info!("TLS UPDATING");
            let _ = tls
                .secret_name
                .get_or_insert(TLS_SERVER_SECRET_NAME.to_string());
            info!("{:?}", tls);

            Ok(((config, policy), Some((proxy_addr, tls))))
        } else {
            Ok(((config, policy), None))
        }
    }

    pub fn parse_k8s_cli_or_exit(self) -> (Config, K8Config, Option<(String, TlsConfig)>) {
        match self.get_sc_and_k8_config() {
            Err(err) => {
                print_cli_err!(err);
                process::exit(-1);
            }
            Ok(config) => config,
        }
    }

    pub fn parse_cli_or_exit(self) -> (Config, Option<(String, TlsConfig)>) {
        match self.as_sc_config() {
            Err(err) => {
                print_cli_err!(err);
                process::exit(-1);
            }
            Ok(config) => config,
        }
    }
}

#[derive(Debug, Parser, Clone, Default)]
pub struct TlsConfig {
    /// enable tls
    #[arg(long)]
    tls: bool,

    /// TLS: path to server certificate
    #[arg(long)]
    pub server_cert: Option<String>,

    #[arg(long)]
    /// TLS: path to server private key
    pub server_key: Option<String>,

    /// TLS: enable client cert
    #[arg(long)]
    pub enable_client_cert: bool,

    /// TLS: path to ca cert, required when client cert is enabled
    #[arg(long)]
    pub ca_cert: Option<String>,

    #[arg(long)]
    /// TLS: address of non tls public service, required
    bind_non_tls_public: Option<String>,

    #[arg(long)]
    /// Secret name used while adding to kubernetes
    pub secret_name: Option<String>,
}

impl TlsConfig {
    pub fn try_build_tls_acceptor(&self) -> Result<TlsAcceptor, IoError> {
        let server_crt_path = self
            .server_cert
            .as_ref()
            .ok_or_else(|| IoError::new(ErrorKind::NotFound, "missing server cert"))?;
        info!("using server crt: {}", server_crt_path);
        let server_key_path = self
            .server_key
            .as_ref()
            .ok_or_else(|| IoError::new(ErrorKind::NotFound, "missing server key"))?;
        info!("using server key: {}", server_key_path);

        let builder = (if self.enable_client_cert {
            let ca_path = self
                .ca_cert
                .as_ref()
                .ok_or_else(|| IoError::new(ErrorKind::NotFound, "missing ca cert"))?;
            info!("using client cert CA path: {}", ca_path);
            TlsAcceptor::builder()
                .map_err(|err| err.into_io_error())?
                .with_ssl_verify_mode(SslVerifyMode::PEER)
                .with_ca_from_pem_file(ca_path)
                .map_err(|err| err.into_io_error())?
        } else {
            info!("using tls anonymous access");
            TlsAcceptor::builder().map_err(|err| err.into_io_error())?
        })
        .with_certifiate_and_key_from_pem_files(server_crt_path, server_key_path)
        .map_err(|err| err.into_io_error())?;

        Ok(builder.build())
    }
}
