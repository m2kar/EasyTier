use dashmap::DashMap;

use crate::proto::rpc_types;
use crate::proto::rpc_types::descriptor::ServiceDescriptor;
use crate::proto::rpc_types::handler::{Handler, HandlerExt};

use super::RpcController;

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
    ) -> rpc_types::error::Result<bytes::Bytes> {
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
    ) -> rpc_types::error::Result<bytes::Bytes> {
        let entry =
            self.table
                .get(service_key)
                .ok_or(rpc_types::error::Error::InvalidServiceKey(
                    service_key.service_name.clone(),
                    service_key.proto_name.clone(),
                ))?;
        entry.call_method(ctrl, method_index, input).await
    }
}
