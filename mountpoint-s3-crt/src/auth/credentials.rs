//! AWS credentials providers

use std::fmt::Debug;
use std::ptr::NonNull;
use std::time::{Instant, SystemTime};
use std::u64;

use aws_credential_types::provider::ProvideCredentials;
use mountpoint_s3_crt_sys::{
    aws_common_error, aws_credentials_new_with_options, aws_credentials_options, aws_credentials_provider,
    aws_credentials_provider_acquire, aws_credentials_provider_cached_options,
    aws_credentials_provider_chain_default_options, aws_credentials_provider_delegate_options,
    aws_credentials_provider_new_anonymous, aws_credentials_provider_new_cached,
    aws_credentials_provider_new_chain_default, aws_credentials_provider_new_delegate,
    aws_credentials_provider_new_profile, aws_credentials_provider_new_static,
    aws_credentials_provider_profile_options, aws_credentials_provider_release,
    aws_credentials_provider_static_options, aws_on_get_credentials_callback_fn, AWS_OP_ERR, AWS_OP_SUCCESS,
};

use crate::auth::auth_library_init;
use crate::common::allocator::Allocator;
use crate::common::error::Error;
use crate::io::channel_bootstrap::ClientBootstrap;
use crate::{CrtError as _, ToAwsByteCursor as _};

/// Options for creating a default credentials provider
#[derive(Debug)]
pub struct CredentialsProviderChainDefaultOptions<'a> {
    /// The client bootstrap this credentials provider should use to setup channels
    pub bootstrap: &'a mut ClientBootstrap,
}

/// Options for creating a profile credentials provider
#[derive(Debug)]
pub struct CredentialsProviderProfileOptions<'a> {
    /// The client bootstrap this credentials provider should use to setup channels
    pub bootstrap: &'a mut ClientBootstrap,
    /// The name of profile to use.
    pub profile_name_override: &'a str,
}

/// Options for creating a static credentials provider
pub struct CredentialsProviderStaticOptions<'a> {
    /// AWS access key ID
    pub access_key_id: &'a str,
    /// AWS secret access key
    pub secret_access_key: &'a str,
    /// AWS session token (only required for some credentials sources, e.g. STS)
    pub session_token: Option<&'a str>,
}

impl Debug for CredentialsProviderStaticOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialsProviderStaticOptions")
            .field("access_key_id", &"** redacted **")
            .field("secret_access_key", &"** redacted **")
            .field("session_token", &self.session_token.map(|_| "** redacted **"))
            .finish()
    }
}

/// A credentials provider is an object that has an asynchronous query function for retrieving AWS
/// credentials
#[derive(Debug)]
pub struct CredentialsProvider {
    pub(crate) inner: NonNull<aws_credentials_provider>,
}

// SAFETY: aws_credentials_provider is thread-safe.
unsafe impl Send for CredentialsProvider {}
// SAFETY: aws_credentials_provider is thread-safe.
unsafe impl Sync for CredentialsProvider {}

impl CredentialsProvider {
    /// Creates the default credential provider chain as used by most AWS SDKs
    pub fn new_chain_default(
        allocator: &Allocator,
        options: CredentialsProviderChainDefaultOptions,
    ) -> Result<Self, Error> {
        auth_library_init(allocator);

        let inner_options = aws_credentials_provider_chain_default_options {
            bootstrap: options.bootstrap.inner.as_ptr(),
            ..Default::default()
        };

        // SAFETY: aws_credentials_provider_new_chain_default makes a copy of the bootstrap options.
        let inner = unsafe {
            aws_credentials_provider_new_chain_default(allocator.inner.as_ptr(), &inner_options).ok_or_last_error()?
        };

        Ok(Self { inner })
    }

    /// Creates the anonymous credential provider.
    /// Anonymous credentials provider gives you anonymous credentials which can be used to skip the signing process.
    pub fn new_anonymous(allocator: &Allocator) -> Result<Self, Error> {
        auth_library_init(allocator);

        // SAFETY: allocator is a valid aws_allocator and shutdown_options is optional
        let inner = unsafe {
            aws_credentials_provider_new_anonymous(allocator.inner.as_ptr(), std::ptr::null_mut()).ok_or_last_error()?
        };

        Ok(Self { inner })
    }

    /// Creates the profile credential provider.
    pub fn new_profile(allocator: &Allocator, options: CredentialsProviderProfileOptions) -> Result<Self, Error> {
        auth_library_init(allocator);

        // SAFETY: aws_credentials_provider_new_profile makes a copy of bootstrap
        // and contents of profile_name_override.
        let inner = unsafe {
            let inner_options = aws_credentials_provider_profile_options {
                bootstrap: options.bootstrap.inner.as_ptr(),
                profile_name_override: options.profile_name_override.as_aws_byte_cursor(),
                ..Default::default()
            };

            aws_credentials_provider_new_profile(allocator.inner.as_ptr(), &inner_options).ok_or_last_error()?
        };

        Ok(Self { inner })
    }

    /// Creates a static credential provider that always returns the given credentials
    pub fn new_static(allocator: &Allocator, options: CredentialsProviderStaticOptions) -> Result<Self, Error> {
        auth_library_init(allocator);

        // SAFETY: aws_credentials_provider_new_static makes a copy of the strings
        let inner = unsafe {
            let inner_options = aws_credentials_provider_static_options {
                access_key_id: options.access_key_id.as_aws_byte_cursor(),
                secret_access_key: options.secret_access_key.as_aws_byte_cursor(),
                session_token: options
                    .session_token
                    .map(|t| t.as_aws_byte_cursor())
                    .unwrap_or_default(),
                ..Default::default()
            };

            aws_credentials_provider_new_static(allocator.inner.as_ptr(), &inner_options).ok_or_last_error()?
        };

        Ok(Self { inner })
    }

    /// Creates a new credential provider using AWS Rust SDK's default provider chain.
    pub fn new_rust_sdk_default_chain(allocator: &Allocator) -> Result<Self, Error> {
        auth_library_init(allocator);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to create Tokio runtime");

        let provide_credentials = rt.block_on(aws_config::default_provider::credentials::default_provider());

        let delegate_provider = unsafe {
            let delegate_user_data = Box::leak(Box::new(CredentialsDelegateUserData {
                allocator: Allocator::default(),
                rt,
                provide_credentials: Box::new(provide_credentials),
            }));

            let inner_options = aws_credentials_provider_delegate_options {
                get_credentials: Some(get_credentials_fn),
                delegate_user_data: delegate_user_data as *mut CredentialsDelegateUserData as *mut libc::c_void,
                ..Default::default()
            };

            aws_credentials_provider_new_delegate(allocator.inner.as_ptr(), &inner_options).ok_or_last_error()?
        };

        let cached = unsafe {
            aws_credentials_provider_new_cached(
                allocator.inner.as_ptr(),
                &aws_credentials_provider_cached_options {
                    source: delegate_provider.as_ptr(),
                    refresh_time_in_milliseconds: 900_000, // Same as `aws_credentials_provider_new_chain_default`, 15 minutes

                    ..Default::default()
                },
            )
            .ok_or_last_error()?
        };

        Ok(Self { inner: cached })
    }
}

impl Clone for CredentialsProvider {
    fn clone(&self) -> Self {
        // SAFETY: `self.inner` is a valid `aws_credentials_provider` for as long as `self` exists
        unsafe {
            aws_credentials_provider_acquire(self.inner.as_ptr());
        }

        Self { inner: self.inner }
    }
}

impl Drop for CredentialsProvider {
    fn drop(&mut self) {
        // SAFETY: `self.inner` is a valid `aws_credentials_provider` and we're in drop so it's safe
        // to decrement the reference count.
        unsafe {
            aws_credentials_provider_release(self.inner.as_ptr());
        }
    }
}

struct CredentialsDelegateUserData {
    allocator: Allocator,
    rt: tokio::runtime::Runtime,
    provide_credentials: Box<dyn ProvideCredentials>,
}

unsafe extern "C" fn get_credentials_fn(
    delegate_user_data: *mut libc::c_void,
    callback: aws_on_get_credentials_callback_fn,
    callback_user_data: *mut libc::c_void,
) -> libc::c_int {
    let delegate_user_data = delegate_user_data as *mut CredentialsDelegateUserData;
    let allocator = &(*delegate_user_data).allocator;
    let rt = &(*delegate_user_data).rt;
    let provide_credentials = &(*delegate_user_data).provide_credentials;

    let Some(callback) = callback else { return AWS_OP_ERR };

    let start = Instant::now();
    let credentials = match rt.block_on(provide_credentials.provide_credentials()) {
        Ok(credentials) => credentials,
        Err(err) => {
            eprintln!("Failed to get credentials {:?}", err);
            return AWS_OP_ERR;
        }
    };

    eprintln!("Fetched credentials in {:?}", start.elapsed());

    callback(
        aws_credentials_new_with_options(
            allocator.inner.as_ptr(),
            &aws_credentials_options {
                access_key_id_cursor: credentials.access_key_id().as_aws_byte_cursor(),
                secret_access_key_cursor: credentials.secret_access_key().as_aws_byte_cursor(),
                session_token_cursor: credentials
                    .session_token()
                    .map(|t| t.as_aws_byte_cursor())
                    .unwrap_or_default(),
                expiration_timepoint_seconds: credentials
                    .expiry()
                    .map(|t| {
                        t.duration_since(SystemTime::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(u64::MAX)
                    })
                    .unwrap_or(u64::MAX),
                ..Default::default()
            },
        ),
        aws_common_error::AWS_ERROR_SUCCESS as i32,
        callback_user_data,
    );
    return AWS_OP_SUCCESS;
}
