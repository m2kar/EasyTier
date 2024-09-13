use std::future::Future;
use std::pin::Pin;

use dashmap::DashMap;

use crate::proto::rpc_types::descriptor::{MethodDescriptor, ServiceDescriptor as _};
use crate::proto::rpc_types::handler::Handler as _;

use super::peer_rpc::*;
use super::rpc_types::handler::{Handler, HandlerExt};

pub type RpcController = super::rpc_types::controller::BaseController;

#[derive(Clone)]
struct GreetingService;

#[async_trait::async_trait]
impl Greeting for GreetingService {
    type Controller = RpcController;
    async fn say_hello(
        &self,
        ctrl: Self::Controller,
        input: SayHelloRequest,
    ) -> crate::proto::rpc_types::error::Result<SayHelloResponse> {
        Ok(SayHelloResponse::default())
    }
    /// Generates a "goodbye" greeting based on the supplied info.
    async fn say_goodbye(
        &self,
        ctrl: Self::Controller,
        input: SayGoodbyeRequest,
    ) -> crate::proto::rpc_types::error::Result<SayGoodbyeResponse> {
        Ok(SayGoodbyeResponse::default())
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Hash)]
struct ServiceKey {
    pub service_name: String,
    pub proto_name: String,
}

struct ServiceEntry {
    name: String,
    service: Box<dyn HandlerExt<Controller = RpcController>>,
}

impl ServiceEntry {
    fn new<H: Handler<Controller = RpcController>>(name: String, h: H) -> Self {
        Self {
            name,
            service: Box::new(h),
        }
    }

    async fn call_method(
        &self,
        ctrl: RpcController,
        method_index: u8,
        input: bytes::Bytes,
    ) -> super::rpc_types::error::Result<bytes::Bytes> {
        self.service.call_method(ctrl, method_index, input).await
    }
}

struct ServiceTable {
    table: DashMap<ServiceKey, ServiceEntry>,
}

impl ServiceTable {
    fn new() -> Self {
        Self {
            table: DashMap::new(),
        }
    }

    fn register<H: Handler<Controller = RpcController>>(&self, h: H) {
        let desc = h.service_descriptor();
        let key = ServiceKey {
            service_name: desc.name().to_string(),
            proto_name: desc.proto_name().to_string(),
        };
        let entry = ServiceEntry::new(key.service_name.clone(), h);
        self.table.insert(key, entry);
    }

    async fn call_method(
        &self,
        service_key: &ServiceKey,
        method_index: u8,
        ctrl: RpcController,
        input: bytes::Bytes,
    ) -> super::rpc_types::error::Result<bytes::Bytes> {
        let entry = self.table.get(service_key).ok_or(
            super::rpc_types::error::Error::InvalidServiceKey(
                service_key.service_name.clone(),
                service_key.proto_name.clone(),
            ),
        )?;
        entry.call_method(ctrl, method_index, input).await
    }
}

#[tokio::test]
async fn rpc_build_test() {
    let table = ServiceTable::new();
    let server = GreetingServer::new(GreetingService {});
    table.register(server);

    let ctrl = RpcController {};
    table.call_method(service_key, 1, ctrl, input);
    println!("{:?}", desc.name());
}
