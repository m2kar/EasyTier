pub trait Controller: Send + Sync + 'static {}

pub struct BaseController {}

impl Controller for BaseController {}
