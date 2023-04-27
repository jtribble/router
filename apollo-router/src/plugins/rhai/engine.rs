use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::SystemTime;

use http::header::HeaderName;
use http::header::HeaderValue;
use http::header::InvalidHeaderName;
use http::uri::Authority;
use http::uri::Parts;
use http::uri::PathAndQuery;
use http::HeaderMap;
use http::Uri;
use rhai::module_resolvers::FileModuleResolver;
use rhai::plugin::*;
use rhai::serde::from_dynamic;
use rhai::serde::to_dynamic;
use rhai::Array;
use rhai::Dynamic;
use rhai::Engine;
use rhai::EvalAltResult;
use rhai::FnPtr;
use rhai::Instant;
use rhai::Map;
use rhai::Scope;
use rhai::AST;
use tower::BoxError;
use uuid::Uuid;

use super::execution;
use super::subgraph;
use super::supergraph;
use super::Rhai;
use super::ServiceStep;
use crate::error::Error;
use crate::graphql::Request;
use crate::graphql::Response;
use crate::http_ext;
use crate::json_ext::Object;
use crate::json_ext::Value;
use crate::plugins::authentication::APOLLO_AUTHENTICATION_JWT_CLAIMS;
use crate::tracer::TraceId;
use crate::Context;

const CANNOT_ACCESS_HEADERS_ON_A_DEFERRED_RESPONSE: &str =
    "cannot access headers on a deferred response";

pub(super) trait OptionDance<T> {
    fn with_mut<R>(&self, f: impl FnOnce(&mut T) -> R) -> R;

    fn replace(&self, f: impl FnOnce(T) -> T);

    fn take_unwrap(self) -> T;
}

pub(super) type SharedMut<T> = rhai::Shared<Mutex<Option<T>>>;

impl<T> OptionDance<T> for SharedMut<T> {
    fn with_mut<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let mut guard = self.lock().expect("poisoned mutex");
        f(guard.as_mut().expect("re-entrant option dance"))
    }

    fn replace(&self, f: impl FnOnce(T) -> T) {
        let mut guard = self.lock().expect("poisoned mutex");
        *guard = Some(f(guard.take().expect("re-entrant option dance")))
    }

    fn take_unwrap(self) -> T {
        match Arc::try_unwrap(self) {
            Ok(mutex) => mutex.into_inner().expect("poisoned mutex"),

            // TODO: Should we assume the Arc refcount is 1
            // and use `try_unwrap().expect("shared ownership")` instead of this fallback ?
            Err(arc) => arc.lock().expect("poisoned mutex").take(),
        }
        .expect("re-entrant option dance")
    }
}

// We have to keep the modules that we export using `export_module` inline because
// error[E0658]: non-inline modules in proc macro input are unstable
#[export_module]
mod router_base64 {

    #[rhai_fn(pure, return_raw)]
    pub(crate) fn decode(input: &mut ImmutableString) -> Result<String, Box<EvalAltResult>> {
        String::from_utf8(base64::decode(input.as_bytes()).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string().into())
    }

    #[rhai_fn(pure)]
    pub(crate) fn encode(input: &mut ImmutableString) -> String {
        base64::encode(input.as_bytes())
    }
}

// We have to keep the modules that we export using `export_module` inline because
// error[E0658]: non-inline modules in proc macro input are unstable
#[export_module]
mod router_plugin {
    // It would be nice to generate get_originating_headers and
    // set_originating_headers for all response types.
    // However, variations in the composition
    // of <Type>Response means this isn't currently possible.
    // We could revisit this later if these structures are re-shaped.

    // The next group of functions are specifically for interacting
    // with the subgraph_request on a SubgraphRequest.
    #[rhai_fn(get = "subgraph", pure, return_raw)]
    pub(crate) fn get_subgraph(
        obj: &mut SharedMut<subgraph::Request>,
    ) -> Result<http_ext::Request<Request>, Box<EvalAltResult>> {
        Ok(obj.with_mut(|request| (&request.subgraph_request).into()))
    }

    #[rhai_fn(set = "subgraph", return_raw)]
    pub(crate) fn set_subgraph(
        obj: &mut SharedMut<subgraph::Request>,
        sub: http_ext::Request<Request>,
    ) -> Result<(), Box<EvalAltResult>> {
        obj.with_mut(|request| {
            request.subgraph_request = sub.inner;
            Ok(())
        })
    }

    #[rhai_fn(get = "headers", pure, return_raw)]
    pub(crate) fn get_subgraph_headers(
        obj: &mut http_ext::Request<Request>,
    ) -> Result<HeaderMap, Box<EvalAltResult>> {
        Ok(obj.headers().clone())
    }

    #[rhai_fn(set = "headers", return_raw)]
    pub(crate) fn set_subgraph_headers(
        obj: &mut http_ext::Request<Request>,
        headers: HeaderMap,
    ) -> Result<(), Box<EvalAltResult>> {
        *obj.headers_mut() = headers;
        Ok(())
    }

    #[rhai_fn(get = "body", pure, return_raw)]
    pub(crate) fn get_subgraph_body(
        obj: &mut http_ext::Request<Request>,
    ) -> Result<Request, Box<EvalAltResult>> {
        Ok(obj.body().clone())
    }

    #[rhai_fn(set = "body", return_raw)]
    pub(crate) fn set_subgraph_body(
        obj: &mut http_ext::Request<Request>,
        body: Request,
    ) -> Result<(), Box<EvalAltResult>> {
        *obj.body_mut() = body;
        Ok(())
    }

    #[rhai_fn(get = "uri", pure, return_raw)]
    pub(crate) fn get_subgraph_uri(
        obj: &mut http_ext::Request<Request>,
    ) -> Result<Uri, Box<EvalAltResult>> {
        Ok(obj.uri().clone())
    }

    #[rhai_fn(set = "uri", return_raw)]
    pub(crate) fn set_subgraph_uri(
        obj: &mut http_ext::Request<Request>,
        uri: Uri,
    ) -> Result<(), Box<EvalAltResult>> {
        *obj.uri_mut() = uri;
        Ok(())
    }
    // End of SubgraphRequest specific section

    #[rhai_fn(get = "headers", pure, return_raw)]
    pub(crate) fn get_originating_headers_supergraph_response(
        obj: &mut SharedMut<supergraph::Response>,
    ) -> Result<HeaderMap, Box<EvalAltResult>> {
        Ok(obj.with_mut(|response| response.response.headers().clone()))
    }

    #[rhai_fn(name = "is_primary", pure)]
    pub(crate) fn supergraph_response_is_primary(
        _obj: &mut SharedMut<supergraph::Response>,
    ) -> bool {
        true
    }

    #[rhai_fn(get = "headers", pure, return_raw)]
    pub(crate) fn get_originating_headers_router_deferred_response(
        _obj: &mut SharedMut<supergraph::DeferredResponse>,
    ) -> Result<HeaderMap, Box<EvalAltResult>> {
        Err(CANNOT_ACCESS_HEADERS_ON_A_DEFERRED_RESPONSE.into())
    }

    #[rhai_fn(name = "is_primary", pure)]
    pub(crate) fn supergraph_deferred_response_is_primary(
        _obj: &mut SharedMut<supergraph::DeferredResponse>,
    ) -> bool {
        false
    }

    #[rhai_fn(get = "headers", pure, return_raw)]
    pub(crate) fn get_originating_headers_execution_response(
        obj: &mut SharedMut<execution::Response>,
    ) -> Result<HeaderMap, Box<EvalAltResult>> {
        Ok(obj.with_mut(|response| response.response.headers().clone()))
    }

    #[rhai_fn(name = "is_primary", pure)]
    pub(crate) fn execution_response_is_primary(_obj: &mut SharedMut<execution::Response>) -> bool {
        true
    }

    #[rhai_fn(get = "headers", pure, return_raw)]
    pub(crate) fn get_originating_headers_execution_deferred_response(
        _obj: &mut SharedMut<execution::DeferredResponse>,
    ) -> Result<HeaderMap, Box<EvalAltResult>> {
        Err(CANNOT_ACCESS_HEADERS_ON_A_DEFERRED_RESPONSE.into())
    }

    #[rhai_fn(name = "is_primary", pure)]
    pub(crate) fn execution_deferred_response_is_primary(
        _obj: &mut SharedMut<execution::DeferredResponse>,
    ) -> bool {
        false
    }

    #[rhai_fn(get = "headers", pure, return_raw)]
    pub(crate) fn get_originating_headers_subgraph_response(
        obj: &mut SharedMut<subgraph::Response>,
    ) -> Result<HeaderMap, Box<EvalAltResult>> {
        Ok(obj.with_mut(|response| response.response.headers().clone()))
    }

    #[rhai_fn(get = "body", pure, return_raw)]
    pub(crate) fn get_originating_body_supergraph_response(
        obj: &mut SharedMut<supergraph::Response>,
    ) -> Result<Response, Box<EvalAltResult>> {
        Ok(obj.with_mut(|response| response.response.body().clone()))
    }

    #[rhai_fn(get = "body", pure, return_raw)]
    pub(crate) fn get_originating_body_execution_response(
        obj: &mut SharedMut<execution::Response>,
    ) -> Result<Response, Box<EvalAltResult>> {
        Ok(obj.with_mut(|response| response.response.body().clone()))
    }

    #[rhai_fn(get = "body", pure, return_raw)]
    pub(crate) fn get_originating_body_subgraph_response(
        obj: &mut SharedMut<subgraph::Response>,
    ) -> Result<Response, Box<EvalAltResult>> {
        Ok(obj.with_mut(|response| response.response.body().clone()))
    }

    #[rhai_fn(get = "body", pure, return_raw)]
    pub(crate) fn get_originating_body_router_deferred_response(
        obj: &mut SharedMut<supergraph::DeferredResponse>,
    ) -> Result<Response, Box<EvalAltResult>> {
        Ok(obj.with_mut(|response| response.response.clone()))
    }

    #[rhai_fn(get = "body", pure, return_raw)]
    pub(crate) fn get_originating_body_execution_deferred_response(
        obj: &mut SharedMut<execution::DeferredResponse>,
    ) -> Result<Response, Box<EvalAltResult>> {
        Ok(obj.with_mut(|response| response.response.clone()))
    }

    #[rhai_fn(set = "headers", return_raw)]
    pub(crate) fn set_originating_headers_supergraph_response(
        obj: &mut SharedMut<supergraph::Response>,
        headers: HeaderMap,
    ) -> Result<(), Box<EvalAltResult>> {
        obj.with_mut(|response| *response.response.headers_mut() = headers);
        Ok(())
    }

    #[rhai_fn(set = "headers", return_raw)]
    pub(crate) fn set_originating_headers_router_deferred_response(
        _obj: &mut SharedMut<supergraph::DeferredResponse>,
        _headers: HeaderMap,
    ) -> Result<(), Box<EvalAltResult>> {
        Err(CANNOT_ACCESS_HEADERS_ON_A_DEFERRED_RESPONSE.into())
    }

    #[rhai_fn(set = "headers", return_raw)]
    pub(crate) fn set_originating_headers_execution_response(
        obj: &mut SharedMut<execution::Response>,
        headers: HeaderMap,
    ) -> Result<(), Box<EvalAltResult>> {
        obj.with_mut(|response| *response.response.headers_mut() = headers);
        Ok(())
    }

    #[rhai_fn(set = "headers", return_raw)]
    pub(crate) fn set_originating_headers_execution_deferred_response(
        _obj: &mut SharedMut<execution::DeferredResponse>,
        _headers: HeaderMap,
    ) -> Result<(), Box<EvalAltResult>> {
        Err(CANNOT_ACCESS_HEADERS_ON_A_DEFERRED_RESPONSE.into())
    }

    #[rhai_fn(set = "headers", return_raw)]
    pub(crate) fn set_originating_headers_subgraph_response(
        obj: &mut SharedMut<subgraph::Response>,
        headers: HeaderMap,
    ) -> Result<(), Box<EvalAltResult>> {
        obj.with_mut(|response| *response.response.headers_mut() = headers);
        Ok(())
    }

    #[rhai_fn(set = "body", return_raw)]
    pub(crate) fn set_originating_body_supergraph_response(
        obj: &mut SharedMut<supergraph::Response>,
        body: Response,
    ) -> Result<(), Box<EvalAltResult>> {
        obj.with_mut(|response| *response.response.body_mut() = body);
        Ok(())
    }

    #[rhai_fn(set = "body", return_raw)]
    pub(crate) fn set_originating_body_execution_response(
        obj: &mut SharedMut<execution::Response>,
        body: Response,
    ) -> Result<(), Box<EvalAltResult>> {
        obj.with_mut(|response| *response.response.body_mut() = body);
        Ok(())
    }

    #[rhai_fn(set = "body", return_raw)]
    pub(crate) fn set_originating_body_subraph_response(
        obj: &mut SharedMut<subgraph::Response>,
        body: Response,
    ) -> Result<(), Box<EvalAltResult>> {
        obj.with_mut(|response| *response.response.body_mut() = body);
        Ok(())
    }

    #[rhai_fn(set = "body", return_raw)]
    pub(crate) fn set_originating_body_router_deferred_response(
        obj: &mut SharedMut<supergraph::DeferredResponse>,
        body: Response,
    ) -> Result<(), Box<EvalAltResult>> {
        obj.with_mut(|response| response.response = body);
        Ok(())
    }

    #[rhai_fn(set = "body", return_raw)]
    pub(crate) fn set_originating_body_execution_deferred_response(
        obj: &mut SharedMut<execution::DeferredResponse>,
        body: Response,
    ) -> Result<(), Box<EvalAltResult>> {
        obj.with_mut(|response| response.response = body);
        Ok(())
    }

    pub(crate) fn map_request(rhai_service: &mut RhaiService, callback: FnPtr) {
        rhai_service
            .service
            .map_request(rhai_service.clone(), callback)
    }

    pub(crate) fn map_response(rhai_service: &mut RhaiService, callback: FnPtr) {
        rhai_service
            .service
            .map_response(rhai_service.clone(), callback)
    }
}

#[derive(Default)]
pub(crate) struct RhaiExecutionResponse {
    pub(crate) context: Context,
    pub(crate) response: http_ext::Response<Response>,
}

#[derive(Default)]
pub(crate) struct RhaiExecutionDeferredResponse {
    pub(crate) context: Context,
    pub(crate) response: Response,
}

#[derive(Default)]
pub(crate) struct RhaiSupergraphResponse {
    pub(crate) context: Context,
    pub(crate) response: http_ext::Response<Response>,
}

#[derive(Default)]
pub(crate) struct RhaiSupergraphDeferredResponse {
    pub(crate) context: Context,
    pub(crate) response: Response,
}

macro_rules! if_subgraph {
    ( subgraph => $subgraph: block else $not_subgraph: block ) => {
        $subgraph
    };
    ( $base: ident => $subgraph: block else $not_subgraph: block ) => {
        $not_subgraph
    };
}

macro_rules! register_rhai_interface {
    ($engine: ident, $($base: ident), *) => {
        $(
            // Context stuff
            $engine.register_get(
                "context",
                |obj: &mut SharedMut<$base::Request>| -> Result<Context, Box<EvalAltResult>> {
                    Ok(obj.with_mut(|request| request.context.clone()))
                }
            )
            .register_get(
                "context",
                |obj: &mut SharedMut<$base::Response>| -> Result<Context, Box<EvalAltResult>> {
                    Ok(obj.with_mut(|response| response.context.clone()))
                }
            );

            $engine.register_set(
                "context",
                |obj: &mut SharedMut<$base::Request>, context: Context| {
                    obj.with_mut(|request| request.context = context);
                    Ok(())
                }
            )
            .register_set(
                "context",
                |obj: &mut SharedMut<$base::Response>, context: Context| {
                    obj.with_mut(|response| response.context = context);
                    Ok(())
                }
            );

            // Originating Request
            $engine.register_get(
                "headers",
                |obj: &mut SharedMut<$base::Request>| -> Result<HeaderMap, Box<EvalAltResult>> {
                    Ok(obj.with_mut(|request| request.supergraph_request.headers().clone()))
                }
            );

            $engine.register_set(
                "headers",
                |obj: &mut SharedMut<$base::Request>, headers: HeaderMap| {
                    if_subgraph! {
                        $base => {
                            let _unused = (obj, headers);
                            Err("cannot mutate originating request on a subgraph".into())
                        } else {
                            obj.with_mut(|request| *request.supergraph_request.headers_mut() = headers);
                            Ok(())
                        }
                    }
                }
            );

            $engine.register_get(
                "body",
                |obj: &mut SharedMut<$base::Request>| -> Result<Request, Box<EvalAltResult>> {
                    Ok(obj.with_mut(|request| request.supergraph_request.body().clone()))
                }
            );

            $engine.register_set(
                "body",
                |obj: &mut SharedMut<$base::Request>, body: Request| {
                    if_subgraph! {
                        $base => {
                            let _unused = (obj, body);
                            Err("cannot mutate originating request on a subgraph".into())
                        } else {
                            obj.with_mut(|request| *request.supergraph_request.body_mut() = body);
                            Ok(())
                        }
                    }
                }
            );

            $engine.register_get(
                "uri",
                |obj: &mut SharedMut<$base::Request>| -> Result<Uri, Box<EvalAltResult>> {
                    Ok(obj.with_mut(|request| request.supergraph_request.uri().clone()))
                }
            );

            $engine.register_set(
                "uri",
                |obj: &mut SharedMut<$base::Request>, uri: Uri| {
                    if_subgraph! {
                        $base => {
                            let _unused = (obj, uri);
                            Err("cannot mutate originating request on a subgraph".into())
                        } else {
                            obj.with_mut(|request| *request.supergraph_request.uri_mut() = uri);
                            Ok(())
                        }
                    }
                }
            );
        )*
    };
}

#[derive(Clone, Debug)]
pub(crate) struct RhaiService {
    pub(super) scope: Arc<Mutex<Scope<'static>>>,
    pub(super) service: ServiceStep,
    pub(super) engine: Arc<Engine>,
    pub(super) ast: AST,
}

impl Rhai {
    pub(super) fn run_rhai_service(
        &self,
        function_name: &str,
        subgraph: Option<&str>,
        service: ServiceStep,
        scope: Arc<Mutex<Scope<'static>>>,
    ) -> Result<(), String> {
        let block = self.block.load();
        let rhai_service = RhaiService {
            scope: scope.clone(),
            service,
            engine: block.engine.clone(),
            ast: block.ast.clone(),
        };
        let mut guard = scope.lock().unwrap();
        // Note: We don't use `process_error()` here, because this code executes in the context of
        // the pipeline processing. We can't return an HTTP error, we can only return a boxed
        // service which represents the next stage of the pipeline.
        // We could have an error pipeline which always returns results, but that's a big
        // change and one that requires more thought in the future.
        match subgraph {
            Some(name) => {
                block
                    .engine
                    .call_fn(
                        &mut guard,
                        &block.ast,
                        function_name,
                        (rhai_service, name.to_string()),
                    )
                    .map_err(|err| err.to_string())?;
            }
            None => {
                block
                    .engine
                    .call_fn(&mut guard, &block.ast, function_name, (rhai_service,))
                    .map_err(|err| err.to_string())?;
            }
        }

        Ok(())
    }

    pub(super) fn new_rhai_engine(path: Option<PathBuf>, sdl: String) -> Engine {
        let mut engine = Engine::new();
        // If we pass in a path, use it to configure our engine
        // with a FileModuleResolver which allows import to work
        // in scripts.
        if let Some(scripts) = path {
            let resolver = FileModuleResolver::new_with_path(scripts);
            engine.set_module_resolver(resolver);
        }

        // The macro call creates a Rhai module from the plugin module.
        let module = exported_module!(router_plugin);

        let base64_module = exported_module!(router_base64);

        // Configure our engine for execution
        engine
            .set_max_expr_depths(0, 0)
            .on_print(move |rhai_log| {
                tracing::info!("{}", rhai_log);
            })
            // Register our plugin module
            .register_global_module(module.into())
            // Register our base64 module (not global)
            .register_static_module("base64", base64_module.into())
            // Register types accessible in plugin scripts
            .register_type::<Context>()
            .register_type::<HeaderMap>()
            .register_type::<Option<HeaderName>>()
            .register_type::<HeaderName>()
            .register_type::<HeaderValue>()
            .register_type::<(Option<HeaderName>, HeaderValue)>()
            .register_type::<Request>()
            .register_type::<Object>()
            .register_type::<Response>()
            .register_type::<Value>()
            .register_type::<Error>()
            .register_type::<Uri>()
            .register_type::<TraceId>()
            // Register HeaderMap as an iterator so we can loop over contents
            .register_iterator::<HeaderMap>()
            // Register a contains function for HeaderMap so that "in" works
            .register_fn("contains", |x: &mut HeaderMap, key: &str| -> bool {
                match HeaderName::from_str(key) {
                    Ok(hn) => x.contains_key(hn),
                    Err(_e) => false,
                }
            })
            // Register a contains function for Context so that "in" works
            .register_fn("contains", |x: &mut Context, key: &str| -> bool {
                x.get(key).map_or(false, |v: Option<Dynamic>| v.is_some())
            })
            // Register urlencode/decode functions
            .register_fn("urlencode", |x: &mut ImmutableString| -> String {
                urlencoding::encode(x).into_owned()
            })
            .register_fn(
                "urldecode",
                |x: &mut ImmutableString| -> Result<String, Box<EvalAltResult>> {
                    Ok(urlencoding::decode(x)
                        .map_err(|e| e.to_string())?
                        .into_owned())
                },
            )
            .register_fn(
                "headers_are_available",
                |_: &mut SharedMut<supergraph::Response>| -> bool { true },
            )
            .register_fn(
                "headers_are_available",
                |_: &mut SharedMut<supergraph::DeferredResponse>| -> bool { false },
            )
            .register_fn(
                "headers_are_available",
                |_: &mut SharedMut<execution::Response>| -> bool { true },
            )
            .register_fn(
                "headers_are_available",
                |_: &mut SharedMut<execution::DeferredResponse>| -> bool { false },
            )
            // Register a HeaderMap indexer so we can get/set headers
            .register_indexer_get(
                |x: &mut HeaderMap, key: &str| -> Result<String, Box<EvalAltResult>> {
                    let search_name =
                        HeaderName::from_str(key).map_err(|e: InvalidHeaderName| e.to_string())?;
                    Ok(x.get(search_name)
                        .ok_or("")?
                        .to_str()
                        .map_err(|e| e.to_string())?
                        .to_string())
                },
            )
            .register_indexer_set(|x: &mut HeaderMap, key: &str, value: &str| {
                x.insert(
                    HeaderName::from_str(key).map_err(|e| e.to_string())?,
                    HeaderValue::from_str(value).map_err(|e| e.to_string())?,
                );
                Ok(())
            })
            // Register an additional getter which allows us to get multiple values for the same
            // key.
            // Note: We can't register this as an indexer, because that would simply override the
            // existing one, which would break code. When router 2.0 is released, we should replace
            // the existing indexer_get for HeaderMap with this function and mark it as an
            // incompatible change.
            .register_fn("values",
                |x: &mut HeaderMap, key: &str| -> Result<Array, Box<EvalAltResult>> {
                    let search_name =
                        HeaderName::from_str(key).map_err(|e: InvalidHeaderName| e.to_string())?;
                    let mut response = Array::new();
                    for value in x.get_all(search_name).iter() {
                        response.push(value
                            .to_str()
                            .map_err(|e| e.to_string())?
                            .to_string()
                            .into())
                    }
                    Ok(response)
                }
            )
            // Register an additional setter which allows us to set multiple values for the same
            // key.
            .register_indexer_set(|x: &mut HeaderMap, key: &str, value: Array| {
                let h_key = HeaderName::from_str(key).map_err(|e| e.to_string())?;
                for v in value {
                    x.append(
                        h_key.clone(),
                        HeaderValue::from_str(&v.into_string()?).map_err(|e| e.to_string())?,
                    );
                }
                Ok(())
            })
            // Register a Context indexer so we can get/set context
            .register_indexer_get(
                |x: &mut Context, key: &str| -> Result<Dynamic, Box<EvalAltResult>> {
                    x.get(key)
                        .map(|v: Option<Dynamic>| v.unwrap_or(Dynamic::UNIT))
                        .map_err(|e: BoxError| e.to_string().into())
                },
            )
            .register_indexer_set(|x: &mut Context, key: &str, value: Dynamic| {
                let _= x.insert(key, value)
                    .map(|v: Option<Dynamic>| v.unwrap_or(Dynamic::UNIT))
                    .map_err(|e: BoxError| e.to_string())?;
                Ok(())
            })
            // Register Context.upsert()
            .register_fn(
                "upsert",
                |context: NativeCallContext,
                 x: &mut Context,
                 key: &str,
                 callback: FnPtr|
                 -> Result<(), Box<EvalAltResult>> {
                    x.upsert(key, |v: Dynamic| -> Dynamic {
                        // Note: Context::upsert() does not allow the callback to fail, although it
                        // can. If call_within_context() fails, return the original provided
                        // value.
                        callback
                            .call_within_context(&context, (v.clone(),))
                            .unwrap_or(v)
                    })
                    .map_err(|e: BoxError| e.to_string().into())
                },
            )
            // Register get for Header Name/Value from a tuple pair
            .register_get("name", |x: &mut (Option<HeaderName>, HeaderValue)| {
                x.0.clone()
            })
            .register_get("value", |x: &mut (Option<HeaderName>, HeaderValue)| {
                x.1.clone()
            })
            // Request.query
            .register_get("query", |x: &mut Request| {
                x.query.clone().map_or(Dynamic::UNIT, Dynamic::from)
            })
            .register_set("query", |x: &mut Request, value: &str| {
                x.query = Some(value.to_string());
            })
            // Request.operation_name
            .register_get("operation_name", |x: &mut Request| {
                x.operation_name
                    .clone()
                    .map_or(Dynamic::UNIT, Dynamic::from)
            })
            .register_set("operation_name", |x: &mut Request, value: &str| {
                x.operation_name = Some(value.to_string());
            })
            // Request.variables
            .register_get("variables", |x: &mut Request| {
                to_dynamic(x.variables.clone())
            })
            .register_set("variables", |x: &mut Request, om: Map| {
                x.variables = from_dynamic(&om.into())?;
                Ok(())
            })
            // Request.extensions
            .register_get("extensions", |x: &mut Request| {
                to_dynamic(x.extensions.clone())
            })
            .register_set("extensions", |x: &mut Request, om: Map| {
                x.extensions = from_dynamic(&om.into())?;
                Ok(())
            })
            // Request.uri.path
            .register_get("path", |x: &mut Uri| to_dynamic(x.path()))
            .register_set("path", |x: &mut Uri, value: &str| {
                // Because there is no simple way to update parts on an existing
                // Uri (no parts_mut()), then we need to create a new Uri from our
                // existing parts, preserving any query, and update our existing
                // Uri.
                let mut parts: Parts = x.clone().into_parts();
                parts.path_and_query = match parts
                    .path_and_query
                    .ok_or("path and query are missing")?
                    .query()
                {
                    Some(query) => Some(
                        PathAndQuery::from_maybe_shared(format!("{value}?{query}"))
                            .map_err(|e| e.to_string())?,
                    ),
                    None => Some(PathAndQuery::from_str(value).map_err(|e| e.to_string())?),
                };
                *x = Uri::from_parts(parts).map_err(|e| e.to_string())?;
                Ok(())
            })
            // Request.uri.host
            .register_get("host", |x: &mut Uri| to_dynamic(x.host()))
            .register_set("host", |x: &mut Uri, value: &str| {
                // Because there is no simple way to update parts on an existing
                // Uri (no parts_mut()), then we need to create a new Uri from our
                // existing parts, preserving any port, and update our existing
                // Uri.
                let mut parts: Parts = x.clone().into_parts();
                let new_authority = match parts.authority {
                    Some(old_authority) => {
                        if let Some(port) = old_authority.port() {
                            Authority::from_maybe_shared(format!("{value}:{port}"))
                                .map_err(|e| e.to_string())?
                        } else {
                            Authority::from_str(value).map_err(|e| e.to_string())?
                        }
                    }
                    None => Authority::from_str(value).map_err(|e| e.to_string())?,
                };
                parts.authority = Some(new_authority);
                *x = Uri::from_parts(parts).map_err(|e| e.to_string())?;
                Ok(())
            })
            // Response.label
            .register_get("label", |x: &mut Response| {
                x.label.clone().map_or(Dynamic::UNIT, Dynamic::from)
            })
            .register_set("label", |x: &mut Response, value: &str| {
                x.label = Some(value.to_string());
            })
            // Response.data
            .register_get("data", |x: &mut Response| to_dynamic(x.data.clone()))
            .register_set("data", |x: &mut Response, om: Map| {
                x.data = from_dynamic(&om.into())?;
                Ok(())
            })
            // Response.path (Not Implemented)
            // Response.errors
            .register_get("errors", |x: &mut Response| to_dynamic(x.errors.clone()))
            .register_set("errors", |x: &mut Response, value: Dynamic| {
                x.errors = from_dynamic(&value)?;
                Ok(())
            })
            // Response.extensions
            .register_get("extensions", |x: &mut Response| {
                to_dynamic(x.extensions.clone())
            })
            .register_set("extensions", |x: &mut Response, om: Map| {
                x.extensions = from_dynamic(&om.into())?;
                Ok(())
            })
            // TraceId support
            .register_fn("traceid", || -> Result<TraceId, Box<EvalAltResult>> {
                TraceId::maybe_new().ok_or_else(|| "trace unavailable".into())
            })
            .register_fn("to_string", |id: &mut TraceId| -> String { id.to_string() })
            // Register a series of logging functions
            .register_fn("log_trace", |out: Dynamic| {
                tracing::trace!(%out, "rhai_trace");
            })
            .register_fn("log_debug", |out: Dynamic| {
                tracing::debug!(%out, "rhai_debug");
            })
            .register_fn("log_info", |out: Dynamic| {
                tracing::info!(%out, "rhai_info");
            })
            .register_fn("log_warn", |out: Dynamic| {
                tracing::warn!(%out, "rhai_warn");
            })
            .register_fn("log_error", |out: Dynamic| {
                tracing::error!(%out, "rhai_error");
            })
            // Register a function for printing to stderr
            .register_fn("eprint", |x: &str| {
                eprintln!("{x}");
            })
            // Default representation in rhai is the "type", so
            // we need to register a to_string function for all our registered
            // types so we can interact meaningfully with them.
            .register_fn("to_string", |x: &mut Context| -> String {
                format!("{x:?}")
            })
            .register_fn("to_string", |x: &mut Option<HeaderName>| -> String {
                match x {
                    Some(v) => v.to_string(),
                    None => "None".to_string(),
                }
            })
            .register_fn("to_string", |x: &mut HeaderName| -> String {
                x.to_string()
            })
            .register_fn("to_string", |x: &mut HeaderValue| -> String {
                x.to_str().map_or("".to_string(), |v| v.to_string())
            })
            .register_fn("to_string", |x: &mut HeaderMap| -> String {
                let mut msg = String::new();
                for pair in x.iter() {
                    let line = format!(
                        "{}: {}",
                        pair.0,
                        pair.1.to_str().map_or("".to_string(), |v| v.to_string())
                    );
                    msg.push_str(line.as_ref());
                    msg.push('\n');
                }
                msg
            })
            .register_fn(
                "to_string",
                |x: &mut (Option<HeaderName>, HeaderValue)| -> String {
                    format!(
                        "{}: {}",
                        match &x.0 {
                            Some(v) => v.to_string(),
                            None => "None".to_string(),
                        },
                        x.1.to_str().map_or("".to_string(), |v| v.to_string())
                    )
                },
            )
            .register_fn("to_string", |x: &mut Request| -> String {
                format!("{x:?}")
            })
            .register_fn("to_string", |x: &mut Response| -> String {
                format!("{x:?}")
            })
            .register_fn("to_string", |x: &mut Error| -> String {
                format!("{x:?}")
            })
            .register_fn("to_string", |x: &mut Object| -> String {
                format!("{x:?}")
            })
            .register_fn("to_string", |x: &mut Value| -> String {
                format!("{x:?}")
            })
            .register_fn("to_string", |x: &mut Uri| -> String { format!("{x:?}") })
            .register_fn("uuid_v4", || -> String {
                Uuid::new_v4().to_string()
            })
            .register_fn("unix_now", ||-> Result<i64, Box<EvalAltResult>> {
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map_err(|e| e.to_string().into())
                    .map(|x| x.as_secs() as i64)
            })
            // Add query plan getter to execution request
            .register_get(
                "query_plan",
                |obj: &mut SharedMut<execution::Request>| -> String {
                    obj.with_mut(|request| {
                        request
                            .query_plan
                            .formatted_query_plan
                            .clone()
                            .unwrap_or_default()
                    })
                },
            )
            // Add context getter/setters for deferred responses
            .register_get(
                "context",
                |obj: &mut SharedMut<supergraph::DeferredResponse>| -> Result<Context, Box<EvalAltResult>> {
                    Ok(obj.with_mut(|response| response.context.clone()))
                },
            )
            .register_set(
                "context",
                |obj: &mut SharedMut<supergraph::DeferredResponse>, context: Context| {
                    obj.with_mut(|response| response.context = context);
                    Ok(())
                },
            )
            .register_get(
                "context",
                |obj: &mut SharedMut<execution::DeferredResponse>| -> Result<Context, Box<EvalAltResult>> {
                    Ok(obj.with_mut(|response| response.context.clone()))
                },
            )
            .register_set(
                "context",
                |obj: &mut SharedMut<execution::DeferredResponse>, context: Context| {
                    obj.with_mut(|response| response.context = context);
                    Ok(())
                },
            );
        // Add common getter/setters for different types
        register_rhai_interface!(engine, supergraph, execution, subgraph);

        // Since constants in Rhai don't give us the behaviour we expect, let's create some global
        // variables which we use in a variable resolver when we create our engine.
        // Note: We keep the constants for now, since they are documented.
        let mut global_variables = Map::new();
        global_variables.insert("APOLLO_SDL".into(), sdl.into());
        global_variables.insert("APOLLO_START".into(), Instant::now().into());
        global_variables.insert(
            "APOLLO_AUTHENTICATION_JWT_CLAIMS".into(),
            APOLLO_AUTHENTICATION_JWT_CLAIMS.to_string().into(),
        );

        let shared_globals = Arc::new(global_variables);

        // Register a variable resolver.
        // Note: This API is NOT deprecated, but it is considered volatile and may change in the future.
        #[allow(deprecated)]
        engine.on_var(move |name, _index, _context| {
            match name {
                // Intercept attempts to find "Router" variables and return our "global variables"
                // Note: Wrapped in an Arc to lighten the load of cloning.
                "Router" => Ok(Some((*shared_globals).clone().into())),
                // Return Ok(None) to continue with the normal variable resolution process.
                _ => Ok(None),
            }
        });
        engine
    }

    pub(super) fn ast_has_function(&self, name: &str) -> bool {
        self.block
            .load()
            .ast
            .iter_fn_def()
            .any(|fn_def| fn_def.name == name)
    }
}
