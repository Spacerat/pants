use std::cmp::min;
use std::collections::{BTreeMap, HashSet};
use std::convert::TryInto;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use bazel_protos::{self, call_option};
use bytes::{Bytes, BytesMut};
use futures::compat::{Future01CompatExt, Sink01CompatExt};
use futures::future::{FutureExt, TryFutureExt};
use futures::sink::SinkExt;
use futures01::{future, Future, Stream};
use hashing::Digest;
use log::Level;
use serverset::{retry, Serverset};
use workunit_store::with_workunit;

use super::{BackoffConfig, EntryType};

#[derive(Clone)]
pub struct ByteStore {
  instance_name: Option<String>,
  chunk_size_bytes: usize,
  upload_timeout: Duration,
  rpc_attempts: usize,
  env: Arc<grpcio::Environment>,
  serverset: Serverset<grpcio::Channel>,
  headers: BTreeMap<String, String>,
}

impl fmt::Debug for ByteStore {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "ByteStore(name={:?})", self.instance_name)
  }
}

impl ByteStore {
  pub fn new(
    cas_addresses: Vec<String>,
    instance_name: Option<String>,
    root_ca_certs: Option<Vec<u8>>,
    oauth_bearer_token: Option<String>,
    thread_count: usize,
    chunk_size_bytes: usize,
    upload_timeout: Duration,
    backoff_config: BackoffConfig,
    rpc_retries: usize,
    connection_limit: usize,
  ) -> Result<ByteStore, String> {
    let env = Arc::new(grpcio::Environment::new(thread_count));
    let env2 = env.clone();

    let connect = move |cas_address: &str| {
      let builder = grpcio::ChannelBuilder::new(env2.clone());
      if let Some(ref root_ca_certs) = root_ca_certs {
        let creds = grpcio::ChannelCredentialsBuilder::new()
          .root_cert(root_ca_certs.clone())
          .build();
        builder.secure_connect(cas_address, creds)
      } else {
        builder.connect(cas_address)
      }
    };

    let serverset = Serverset::new(cas_addresses, connect, connection_limit, backoff_config)?;

    Ok(ByteStore {
      instance_name,
      chunk_size_bytes,
      upload_timeout,
      rpc_attempts: rpc_retries + 1,
      env,
      serverset,
      headers: oauth_bearer_token
        .iter()
        .map(|t| {
          (
            String::from("authorization"),
            format!("Bearer {}", t.trim()),
          )
        })
        .collect(),
    })
  }

  async fn with_byte_stream_client<
    Value: Send,
    Fut: std::future::Future<Output = Result<Value, String>>,
    F: Fn(bazel_protos::bytestream_grpc::ByteStreamClient) -> Fut,
  >(
    &self,
    f: F,
  ) -> Result<Value, String> {
    retry::all_errors_immediately(&self.serverset, self.rpc_attempts, move |channel| {
      f(bazel_protos::bytestream_grpc::ByteStreamClient::new(
        channel,
      ))
    })
    .await
  }

  async fn with_cas_client<
    Value: Send,
    Fut: std::future::Future<Output = Result<Value, String>>,
    F: Fn(bazel_protos::remote_execution_grpc::ContentAddressableStorageClient) -> Fut,
  >(
    &self,
    f: F,
  ) -> Result<Value, String> {
    retry::all_errors_immediately(&self.serverset, self.rpc_attempts, move |channel| {
      f(bazel_protos::remote_execution_grpc::ContentAddressableStorageClient::new(channel))
    })
    .await
  }

  pub async fn store_bytes(&self, bytes: &[u8]) -> Result<Digest, String> {
    let len = bytes.len();
    let digest = Digest::of_bytes(&bytes);
    let resource_name = format!(
      "{}/uploads/{}/blobs/{}/{}",
      self.instance_name.clone().unwrap_or_default(),
      uuid::Uuid::new_v4(),
      digest.0,
      digest.1,
    );
    let workunit_name = format!("store_bytes({})", resource_name.clone());
    let metadata = workunit_store::WorkunitMetadata::with_level(Level::Debug);
    let store = self.clone();
    let result_future = self.with_byte_stream_client(move |client| {
      let resource_name = resource_name.clone();
      let store = store.clone();
      async move {
        let (sender, receiver) = client
          .write_opt(call_option(&store.headers, None)?.timeout(store.upload_timeout))
          .map_err(|err| {
            format!(
              "Error attempting to connect to upload digest {:?}: {:?}",
              digest, err
            )
          })?;

        let mut sender = sender.sink_compat();

        let chunk_size_bytes = store.chunk_size_bytes;
        let resource_name = resource_name.clone();
        let mut stream = futures::stream::unfold((0, false), move |(offset, has_sent_any)| {
          if offset >= bytes.len() && has_sent_any {
            futures::future::ready(None)
          } else {
            let mut req = bazel_protos::bytestream::WriteRequest::new();
            req.set_resource_name(resource_name.clone());
            req.set_write_offset(offset as i64);
            let next_offset = min(offset + chunk_size_bytes, bytes.len());
            req.set_finish_write(next_offset == bytes.len());
            req.set_data(Bytes::from(&bytes[offset..next_offset]));
            futures::future::ready(Some((
              Ok((req, grpcio::WriteFlags::default())),
              (next_offset, true),
            )))
          }
        });

        sender.send_all(&mut stream).await.or_else(move |e| {
          match e {
            // Some implementations of the remote execution API early-return if the blob has
            // been concurrently uploaded by another client. In this case, they return a
            // WriteResponse with a committed_size equal to the digest's entire size before
            // closing the stream.
            // Because the server then closes the stream, the client gets an RpcFinished
            // error in this case. We ignore this, and will later on verify that the
            // committed_size we received from the server is equal to the expected one. If
            // these are not equal, the upload will be considered a failure at that point.
            // Whether this type of response will become part of the official API is up for
            // discussion: see
            // https://groups.google.com/d/topic/remote-execution-apis/NXUe3ItCw68/discussion.
            grpcio::Error::RpcFinished(None) => Ok(()),
            e => Err(format!(
              "Error attempting to upload digest {:?}: {:?}",
              digest, e
            )),
          }
        })?;

        // The gRPC library cancels streams on drop; closes must be explicit. Not closing
        // the stream caused the BuildGrid CAS server to generate errors on writes
        // when the stream was cancelled because it was not closed explicitly.
        sender.close().await.or_else(|err| {
          match err {
            // Some servers (e.g., RBE) may have already closed the stream for the early
            // return reason identified previously. Treat this condition as a successful close.
            grpcio::Error::RpcFinished(None) => Ok(()),
            e => Err(format!(
              "Error from server when uploading digest {:?}: {:?}",
              digest, e
            )),
          }
        })?;

        let received = receiver
          .map_err(move |e| {
            format!(
              "Error from server when uploading digest {:?}: {:?}",
              digest, e
            )
          })
          .compat()
          .await?;

        if received.get_committed_size() == len as i64 {
          Ok(digest)
        } else {
          Err(format!(
            "Uploading file with digest {:?}: want committed size {} but got {}",
            digest,
            len,
            received.get_committed_size()
          ))
        }
      }
    });

    if let Some(workunit_state) = workunit_store::get_workunit_state() {
      let store = workunit_state.store;
      with_workunit(store, workunit_name, metadata, result_future, |_, md| md).await
    } else {
      result_future.await
    }
  }

  pub async fn load_bytes_with<
    T: Send + 'static,
    F: Fn(Bytes) -> T + Send + Sync + Clone + 'static,
  >(
    &self,
    _entry_type: EntryType,
    digest: Digest,
    f: F,
  ) -> Result<Option<T>, String> {
    let store = self.clone();
    let resource_name = format!(
      "{}/blobs/{}/{}",
      store.instance_name.clone().unwrap_or_default(),
      digest.0,
      digest.1
    );
    let workunit_name = format!("load_bytes_with({})", resource_name.clone());
    let metadata = workunit_store::WorkunitMetadata::with_level(Level::Debug);
    let result_future = self.with_byte_stream_client(move |client| {
      let resource_name = resource_name.clone();
      let store = store.clone();
      let f = f.clone();
      async move {
        let stream = client
          .read_opt(
            &{
              let mut req = bazel_protos::bytestream::ReadRequest::new();
              req.set_resource_name(resource_name.clone());
              req.set_read_offset(0);
              // 0 means no limit.
              req.set_read_limit(0);
              req
            },
            call_option(&store.headers, None)?,
          )
          .map_err(|err| format!("Error making CAS read request for {:?}: {:?}", digest, err))?;

        let bytes_res = stream
          .fold(BytesMut::with_capacity(digest.1), move |mut bytes, r| {
            bytes.extend_from_slice(&r.data);
            future::ok::<_, grpcio::Error>(bytes)
          })
          .compat()
          .await;

        // We ensure that we hold onto the client until after we've consumed the stream as a
        // workaround for https://github.com/pingcap/grpc-rs/issues/123
        std::mem::drop(client);

        let maybe_bytes = match bytes_res {
          Ok(bytes) => Some(bytes.freeze()),
          Err(grpcio::Error::RpcFailure(grpcio::RpcStatus {
            status: grpcio::RpcStatusCode::NOT_FOUND,
            ..
          })) => None,
          Err(e) => {
            return Err(format!(
              "Error from server in response to CAS read request: {:?}",
              e
            ))
          }
        };

        Ok(maybe_bytes.map(f))
      }
    });

    if let Some(workunit_state) = workunit_store::get_workunit_state() {
      let store = workunit_state.store;
      with_workunit(store, workunit_name, metadata, result_future, |_, md| md).await
    } else {
      result_future.await
    }
  }

  ///
  /// Given a collection of Digests (digests),
  /// returns the set of digests from that collection not present in the CAS.
  ///
  pub fn list_missing_digests(
    &self,
    request: bazel_protos::remote_execution::FindMissingBlobsRequest,
  ) -> impl Future<Item = HashSet<Digest>, Error = String> {
    let store = self.clone();
    let workunit_name = format!(
      "list_missing_digests({})",
      store.instance_name.clone().unwrap_or_default()
    );
    let metadata = workunit_store::WorkunitMetadata::with_level(Level::Debug);
    let result_future = async move {
      let store2 = store.clone();
      store2
        .with_cas_client(move |client| {
          let request = request.clone();
          let store = store.clone();
          async move {
            let response = client
              .find_missing_blobs_opt(&request, call_option(&store.headers, None)?)
              .map_err(|err| {
                format!(
                  "Error from server in response to find_missing_blobs_request: {:?}",
                  err
                )
              })?;
            response
              .get_missing_blob_digests()
              .iter()
              .map(|digest| digest.try_into())
              .collect::<Result<HashSet<_>, _>>()
          }
        })
        .await
    };
    async {
      if let Some(workunit_state) = workunit_store::get_workunit_state() {
        let store = workunit_state.store;
        with_workunit(store, workunit_name, metadata, result_future, |_, md| md).await
      } else {
        result_future.await
      }
    }
    .boxed()
    .compat()
  }

  pub(super) fn find_missing_blobs_request<'a, Digests: Iterator<Item = &'a Digest>>(
    &self,
    digests: Digests,
  ) -> bazel_protos::remote_execution::FindMissingBlobsRequest {
    let mut request = bazel_protos::remote_execution::FindMissingBlobsRequest::new();
    if let Some(ref instance_name) = self.instance_name {
      request.set_instance_name(instance_name.clone());
    }
    for digest in digests {
      request.mut_blob_digests().push(digest.into());
    }
    request
  }
}
