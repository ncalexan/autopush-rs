use autopush_common::db::client::DbClient;

use crate::error::ApiResult;
use crate::extractors::notification::Notification;
use crate::extractors::router_data_input::RouterDataInput;
use crate::routers::common::{build_message_data, handle_error, incr_success_metrics};
use crate::routers::wns::client::WnsClient;
use crate::routers::wns::error::WnsError;
use crate::routers::wns::settings::{WnsServerCredential, WnsSettings};
use crate::routers::{Router, RouterError, RouterResponse};
use async_trait::async_trait;
use cadence::StatsdClient;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use url::Url;
use uuid::Uuid;

/// 28 days
const MAX_TTL: usize = 28 * 24 * 60 * 60;

/// Firebase Cloud Messaging router
pub struct WnsRouter {
    settings: WnsSettings,
    endpoint_url: Url,
    metrics: Arc<StatsdClient>,
    db: Box<dyn DbClient>,
    /// A map from application ID to an authenticated WNS client
    clients: HashMap<String, WnsClient>,
}

impl WnsRouter {
    /// Create a new `WnsRouter`
    pub async fn new(
        settings: WnsSettings,
        endpoint_url: Url,
        http: reqwest::Client,
        metrics: Arc<StatsdClient>,
        db: Box<dyn DbClient>,
    ) -> Result<Self, WnsError> {
        let server_credentials = settings.credentials()?;
        let clients = Self::create_clients(&settings, server_credentials, http.clone())
            .await
            .map_err(WnsError::OAuthClientBuild)?;
        Ok(Self {
            settings,
            endpoint_url,
            metrics,
            db,
            clients,
        })
    }

    /// Create WNS clients for each application
    async fn create_clients(
        settings: &WnsSettings,
        server_credentials: HashMap<String, WnsServerCredential>,
        http: reqwest::Client,
    ) -> std::io::Result<HashMap<String, WnsClient>> {
        let mut clients = HashMap::new();

        for (profile, server_credential) in server_credentials {
            trace!("Inserting client {}: {:?}", profile, server_credential);
            clients.insert(
                profile,
                WnsClient::new(settings, server_credential, http.clone()).await?,
            );
        }
        trace!("Initialized {} WNS clients", clients.len());
        Ok(clients)
    }

    /// if we have any clients defined, this connection is "active"
    pub fn active(&self) -> bool {
        !self.clients.is_empty()
    }

    /// Do the gauntlet check to get the routing credentials, these are the
    /// sender/project ID, and the subscription specific user routing token.
    /// WNS stores the values in the top hash as `token` & `app_id`.
    /// If any of these error out, it's probably because of a corrupted key.
    fn routing_info(
        &self,
        router_data: &HashMap<String, Value>,
        uaid: &Uuid,
    ) -> ApiResult<(String, String)> {
        // let creds = router_data.get("creds").and_then(Value::as_object);
        // // GCM and WNS both should store the client registration_token as token in the router_data.
        // // There was some confusion about router table records that may store the client
        // // routing token in `creds.auth`, but it's believed that this a duplicate of the
        // // server authentication token and can be ignored since we use the value specified
        // // in the settings.
        let routing_token = match router_data.get("token").and_then(Value::as_str) {
            Some(v) => v.to_owned(),
            None => {
                warn!("No Registration token found for user {}", uaid.to_string());
                return Err(WnsError::NoRegistrationToken.into());
            }
        };
        let app_id = match router_data.get("app_id").and_then(Value::as_str) {
            Some(v) => v.to_owned(),
            None => {
                warn!("No App_id found for user {}", uaid.to_string());
                return Err(WnsError::NoAppId.into());
            }
        };
        Ok((routing_token, app_id))
    }
}

#[async_trait(?Send)]
impl Router for WnsRouter {
    fn register(
        &self,
        router_data_input: &RouterDataInput,
        app_id: &str,
    ) -> Result<HashMap<String, Value>, RouterError> {
        trace!("{} in {:?}", app_id, self.clients.keys());
        if !self.clients.contains_key(app_id) {
            return Err(WnsError::InvalidAppId(app_id.to_owned()).into());
        }

        let mut router_data = HashMap::new();
        router_data.insert(
            "token".to_string(),
            serde_json::to_value(&router_data_input.token).unwrap(),
        );
        router_data.insert("app_id".to_string(), serde_json::to_value(app_id).unwrap());

        // TODO: round trip some profile identifier here?  Or maybe
        // map the "chid" provided?

        Ok(router_data)
    }

    async fn route_notification(&self, notification: &Notification) -> ApiResult<RouterResponse> {
        debug!(
            "Sending WNS notification to UAID {}",
            notification.subscription.user.uaid
        );
        trace!("Notification = {:?}", notification);

        let router_data = notification
            .subscription
            .user
            .router_data
            .as_ref()
            .ok_or(WnsError::NoRegistrationToken)?;

        let (routing_token, app_id) =
            self.routing_info(router_data, &notification.subscription.user.uaid)?;
        let ttl = MAX_TTL.min(self.settings.min_ttl.max(notification.headers.ttl as usize));

        // Send the notification to WNS
        let client = self
            .clients
            .get(&app_id)
            .ok_or_else(|| WnsError::InvalidAppId(app_id.clone()))?;

        let message_data = build_message_data(notification)?;
        let platform = "wnsv1";
        trace!("Sending message to {platform}: [{:?}]", &app_id);
        if let Err(e) = client.send(message_data, routing_token, ttl).await {
            trace!("Sending message to {platform}: [{:?}] error {:?}", &app_id, e);
            return Err(handle_error(
                e,
                &self.metrics,
                self.db.as_ref(),
                platform,
                &app_id,
                notification.subscription.user.uaid,
                notification.subscription.vapid.clone(),
            )
            .await);
        };
        incr_success_metrics(&self.metrics, platform, &app_id, notification);
        // Sent successfully, update metrics and make response
        trace!("Send request was successful");

        Ok(RouterResponse::success(
            self.endpoint_url
                .join(&format!("/m/{}", notification.message_id))
                .expect("Message ID is not URL-safe")
                .to_string(),
            notification.headers.ttl as usize,
        ))
    }
}

// #[cfg(test)]
// mod tests {
//     use crate::error::ApiErrorKind;
//     use crate::extractors::routers::RouterType;
//     use crate::routers::common::tests::{make_notification, CHANNEL_ID};
//     use crate::routers::wns::client::tests::{
//         make_service_key, mock_wns_endpoint_builder, mock_token_endpoint, GCM_PROJECT_ID,
//         PROJECT_ID,
//     };
//     use crate::routers::wns::error::WnsError;
//     use crate::routers::wns::router::WnsRouter;
//     use crate::routers::wns::settings::WnsSettings;
//     use crate::routers::RouterError;
//     use crate::routers::{Router, RouterResponse};
//     use autopush_common::db::client::DbClient;
//     use autopush_common::db::mock::MockDbClient;
//     use std::sync::Arc;

//     use cadence::StatsdClient;
//     use mockall::predicate;
//     use std::collections::HashMap;
//     use url::Url;

//     const WNS_TOKEN: &str = "test-token";

//     /// Create a router for testing, using the given service auth file
//     async fn make_router(
//         server: &mut mockito::ServerGuard,
//         wns_credential: String,
//         gcm_credential: String,
//         db: Box<dyn DbClient>,
//     ) -> WnsRouter {
//         let url = &server.url();
//         WnsRouter::new(
//             WnsSettings {
//                 base_url: Url::parse(url).unwrap(),
//                 server_credentials: serde_json::json!({
//                     "dev": {
//                         "project_id": PROJECT_ID,
//                         "credential": wns_credential
//                     },
//                     GCM_PROJECT_ID: {
//                         "project_id": GCM_PROJECT_ID,
//                         "credential": gcm_credential,
//                         "is_gcm": true,
//                     }
//                 })
//                 .to_string(),
//                 ..Default::default()
//             },
//             Url::parse("http://localhost:8080/").unwrap(),
//             reqwest::Client::new(),
//             Arc::new(StatsdClient::from_sink("autopush", cadence::NopMetricSink)),
//             db,
//         )
//         .await
//         .unwrap()
//     }

//     /// Create default user router data
//     fn default_router_data() -> HashMap<String, serde_json::Value> {
//         let mut map = HashMap::new();
//         map.insert(
//             "token".to_string(),
//             serde_json::to_value(WNS_TOKEN).unwrap(),
//         );
//         map.insert("app_id".to_string(), serde_json::to_value("dev").unwrap());
//         map
//     }

//     /// A notification with no data is sent to WNS
//     #[tokio::test]
//     async fn successful_routing_no_data() {
//         let mut server = mockito::Server::new_async().await;

//         let mdb = MockDbClient::new();
//         let db = mdb.into_boxed_arc();
//         let service_key = make_service_key(&server);
//         let router = make_router(&mut server, service_key, "whatever".to_string(), db).await;
//         assert!(router.active());
//         let _token_mock = mock_token_endpoint(&mut server).await;
//         let wns_mock = mock_wns_endpoint_builder(&mut server, PROJECT_ID)
//             .match_body(
//                 serde_json::json!({
//                     "message": {
//                         "android": {
//                             "data": {
//                                 "chid": CHANNEL_ID
//                             },
//                             "ttl": "60s"
//                         },
//                         "token": "test-token"
//                     }
//                 })
//                 .to_string()
//                 .as_str(),
//             )
//             .create();
//         let notification = make_notification(default_router_data(), None, RouterType::WNS);

//         let result = router.route_notification(&notification).await;
//         assert!(result.is_ok(), "result = {result:?}");
//         assert_eq!(
//             result.unwrap(),
//             RouterResponse::success("http://localhost:8080/m/test-message-id".to_string(), 0)
//         );
//         wns_mock.assert();
//     }

//     /// A notification with data is sent to WNS
//     #[tokio::test]
//     async fn successful_routing_with_data() {
//         let mut server = mockito::Server::new_async().await;

//         let mdb = MockDbClient::new();
//         let db = mdb.into_boxed_arc();
//         let service_key = make_service_key(&server);
//         let router = make_router(&mut server, service_key, "whatever".to_string(), db).await;
//         let _token_mock = mock_token_endpoint(&mut server).await;
//         let wns_mock = mock_wns_endpoint_builder(&mut server, PROJECT_ID)
//             .match_body(
//                 serde_json::json!({
//                     "message": {
//                         "android": {
//                             "data": {
//                                 "chid": CHANNEL_ID,
//                                 "body": "test-data",
//                                 "con": "test-encoding",
//                                 "enc": "test-encryption",
//                                 "cryptokey": "test-crypto-key",
//                                 "enckey": "test-encryption-key"
//                             },
//                             "ttl": "60s"
//                         },
//                         "token": "test-token"
//                     }
//                 })
//                 .to_string()
//                 .as_str(),
//             )
//             .create();
//         let data = "test-data".to_string();
//         let notification = make_notification(default_router_data(), Some(data), RouterType::WNS);

//         let result = router.route_notification(&notification).await;
//         assert!(result.is_ok(), "result = {result:?}");
//         assert_eq!(
//             result.unwrap(),
//             RouterResponse::success("http://localhost:8080/m/test-message-id".to_string(), 0)
//         );
//         wns_mock.assert();
//     }

//     /// If there is no client for the user's app ID, an error is returned and
//     /// the WNS request is not sent.
//     #[tokio::test]
//     async fn missing_client() {
//         let mut server = mockito::Server::new_async().await;

//         let db = MockDbClient::new().into_boxed_arc();
//         let service_key = make_service_key(&server);
//         let router = make_router(&mut server, service_key, "whatever".to_string(), db).await;
//         let _token_mock = mock_token_endpoint(&mut server).await;
//         let wns_mock = mock_wns_endpoint_builder(&mut server, PROJECT_ID)
//             .expect(0)
//             .create_async()
//             .await;
//         let mut router_data = default_router_data();
//         let app_id = "app_id".to_string();
//         router_data.insert(
//             app_id.clone(),
//             serde_json::to_value("unknown-app-id").unwrap(),
//         );
//         let notification = make_notification(router_data, None, RouterType::WNS);

//         let result = router.route_notification(&notification).await;
//         assert!(result.is_err());
//         assert!(
//             matches!(
//                 &result.as_ref().unwrap_err().kind,
//                 ApiErrorKind::Router(RouterError::Wns(WnsError::InvalidAppId(_app_id)))
//             ),
//             "result = {result:?}"
//         );
//         wns_mock.assert();
//     }

//     /// If the WNS user no longer exists (404), we drop the user from our database
//     #[tokio::test]
//     async fn no_wns_user() {
//         let mut server = mockito::Server::new_async().await;

//         let notification = make_notification(default_router_data(), None, RouterType::WNS);
//         let mut db = MockDbClient::new();
//         db.expect_remove_user()
//             .with(predicate::eq(notification.subscription.user.uaid))
//             .times(1)
//             .return_once(|_| Ok(()));

//         let service_key = make_service_key(&server);
//         let router = make_router(
//             &mut server,
//             service_key,
//             "whatever".to_string(),
//             db.into_boxed_arc(),
//         )
//         .await;
//         let _token_mock = mock_token_endpoint(&mut server).await;
//         let _wns_mock = mock_wns_endpoint_builder(&mut server, PROJECT_ID)
//             .with_status(404)
//             .with_body(r#"{"error":{"status":"NOT_FOUND","message":"test-message"}}"#)
//             .create_async()
//             .await;

//         let result = router.route_notification(&notification).await;
//         assert!(result.is_err());
//         assert!(
//             matches!(
//                 result.as_ref().unwrap_err().kind,
//                 ApiErrorKind::Router(RouterError::NotFound)
//             ),
//             "result = {result:?}"
//         );
//     }
// }