// Copyright 2024 Oxide Computer Company
//! Routes incoming HTTP requests to handler functions

use super::error::HttpError;
use super::error_status_code::ClientErrorStatusCode;
use super::handler::RouteHandler;

use crate::api_description::ApiEndpointVersions;
use crate::from_map::MapError;
use crate::from_map::MapValue;
use crate::server::ServerContext;
use crate::ApiEndpoint;
use crate::RequestEndpointMetadata;
use http::Method;
use percent_encoding::percent_decode_str;
use semver::Version;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

/// `HttpRouter` is a simple data structure for routing incoming HTTP requests
/// to specific handler functions based on the request method, URI path, and
/// version.  For examples, see the basic test below.
///
/// Routes are registered and looked up according to a path, like `"/foo/bar"`.
/// Paths are split into segments separated by one or more '/' characters.  When
/// registering a route, a path segment may be either a literal string or a
/// variable.  Variables are specified by wrapping the segment in braces.
///
/// For example, a handler registered for `"/foo/bar"` will match only
/// `"/foo/bar"` (after normalization, that is -- it will also match
/// `"/foo///bar"`).  A handler registered for `"/foo/{bar}"` uses a
/// variable for the second segment, so it will match `"/foo/123"` (with `"bar"`
/// assigned to `"123"`) as well as `"/foo/bar456"` (with `"bar"` mapped to
/// `"bar456"`).  Only one segment is matched per variable, so `"/foo/{bar}"`
/// will not match `"/foo/123/456"`.
///
/// The implementation here is essentially a trie where edges represent segments
/// of the URI path.  ("Segments" here are chunks of the path separated by one or
/// more "/" characters.)  To register or look up the path `"/foo/bar/baz"`, we
/// would start at the root and traverse edges for the literal strings `"foo"`,
/// `"bar"`, and `"baz"`, arriving at a particular node.  Each node has a set of
/// handlers, each associated with one HTTP method.
///
/// We make (and, in some cases, enforce) a number of simplifying assumptions.
/// These could be relaxed, but it's not clear that's useful, and enforcing them
/// makes it easier to catch some types of bugs:
///
/// * If a given resource has an edge with a variable name, all routes through
///   this node must use the same name for that variable.  That is, you can't
///   define routes for `"/projects/{id}"` and `"/projects/{project_id}/info"`.
///
/// * A given path cannot use the same variable name twice.  For example, you
///   can't register path `"/projects/{id}/instances/{id}"`.
///
/// * A given resource may have at most one handler for a given HTTP method and
///   version.
///
/// * The expectation is that during server initialization,
///   `HttpRouter::insert()` will be invoked to register a number of route
///   handlers.  After that initialization period, the router will be
///   read-only.  This behavior isn't enforced by `HttpRouter`.
#[derive(Debug)]
pub struct HttpRouter<Context: ServerContext> {
    /// root of the trie
    root: Box<HttpRouterNode<Context>>,
    /// indicates whether this router contains any endpoints that are
    /// constrained by version
    has_versioned_routes: bool,
}

/// Each node in the tree represents a group of HTTP resources having the same
/// handler functions.  As described above, these may correspond to exactly one
/// canonical path (e.g., `"/foo/bar"`) or a set of paths that differ by some
/// number of variable assignments (e.g., `"/projects/123/instances"` and
/// `"/projects/456/instances"`).
///
/// Edges of the tree come in one of type types: edges for literal strings and
/// edges for variable strings.  A given node has either literal string edges or
/// variable edges, but not both.  However, we don't necessarily know what type
/// of outgoing edges a node will have when we create it.
#[derive(Debug)]
struct HttpRouterNode<Context: ServerContext> {
    /// Handlers, etc. for each of the HTTP methods defined for this node.
    methods: BTreeMap<String, Vec<ApiEndpoint<Context>>>,
    /// Edges linking to child nodes.
    literal_edges: Option<BTreeMap<String, Box<HttpRouterNode<Context>>>>,
    variable_edge: Option<(String, Box<HttpRouterNode<Context>>)>,
    rest_edge: Option<(String, Box<HttpRouterNode<Context>>)>,
}

/// `PathSegment` represents a segment in a URI path when the router is being
/// configured.  Each segment may be either a literal string or a variable (the
/// latter indicated by being wrapped in braces). Variables may consume a single
/// /-delimited segment or several as defined by a regex (currently only `.*` is
/// supported).
#[derive(Debug, PartialEq)]
pub enum PathSegment {
    /// a path segment for a literal string
    Literal(String),
    /// a path segment for a variable
    VarnameSegment(String),
    /// a path segment that matches all remaining components for a variable
    VarnameWildcard(String),
}

impl PathSegment {
    /// Given a `&str` representing a path segment from a Uri, return a
    /// PathSegment.  This is used to parse a sequence of path segments to the
    /// corresponding `PathSegment`, which basically means determining whether
    /// it's a variable or a literal.
    pub fn from(segment: &str) -> PathSegment {
        if segment.starts_with('{') || segment.ends_with('}') {
            assert!(
                segment.starts_with('{'),
                "{}",
                "HTTP URI path segment variable missing leading \"{\""
            );
            assert!(
                segment.ends_with('}'),
                "{}",
                "HTTP URI path segment variable missing trailing \"}\""
            );

            let var = &segment[1..segment.len() - 1];

            let (var, pat) = if let Some(index) = var.find(':') {
                (&var[..index], Some(&var[index + 1..]))
            } else {
                (var, None)
            };

            // Note that the only constraint on the variable name is that it is
            // not empty. Consumers may choose odd names like '_' or 'type'
            // that are not valid Rust identifiers and rename them with
            // serde attributes during deserialization.
            assert!(
                !var.is_empty(),
                "HTTP URI path segment variable name must not be empty",
            );

            if let Some(pat) = pat {
                assert!(
                    pat == ".*",
                    "Only the pattern '.*' is currently supported"
                );
                PathSegment::VarnameWildcard(var.to_string())
            } else {
                PathSegment::VarnameSegment(var.to_string())
            }
        } else {
            PathSegment::Literal(segment.to_string())
        }
    }
}

/// Wrapper for a path that's the result of user input i.e. an HTTP query.
/// We use this type to avoid confusion with paths used to define routes.
#[derive(Debug, Clone, Copy)]
pub struct InputPath<'a>(&'a str);

impl<'a> From<&'a str> for InputPath<'a> {
    fn from(s: &'a str) -> Self {
        Self(s)
    }
}

/// A value for a variable which may either be a single value or a list of
/// values in the case of wildcard path matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariableValue {
    String(String),
    Components(Vec<String>),
}

pub type VariableSet = BTreeMap<String, VariableValue>;

impl MapValue for VariableValue {
    fn as_value(&self) -> Result<&str, MapError> {
        match self {
            VariableValue::String(s) => Ok(s.as_str()),
            VariableValue::Components(_) => Err(MapError(
                "cannot deserialize sequence as a single value".to_string(),
            )),
        }
    }

    fn as_seq(&self) -> Result<Box<dyn Iterator<Item = String>>, MapError> {
        match self {
            VariableValue::String(_) => Err(MapError(
                "cannot deserialize a single value as a sequence".to_string(),
            )),
            VariableValue::Components(v) => Ok(Box::new(v.clone().into_iter())),
        }
    }
}

/// The result of invoking `HttpRouter::lookup_route()`.
///
/// A successful route lookup includes the handler and endpoint-related metadata.
#[derive(Debug)]
pub struct RouterLookupResult<Context: ServerContext> {
    pub handler: Arc<dyn RouteHandler<Context>>,
    pub endpoint: RequestEndpointMetadata,
}

impl<Context: ServerContext> HttpRouterNode<Context> {
    pub fn new() -> Self {
        HttpRouterNode {
            methods: BTreeMap::new(),
            literal_edges: None,
            variable_edge: None,
            rest_edge: None,
        }
    }
}

impl<Context: ServerContext> HttpRouter<Context> {
    /// Returns a new `HttpRouter` with no routes configured.
    pub fn new() -> Self {
        HttpRouter {
            root: Box::new(HttpRouterNode::new()),
            has_versioned_routes: false,
        }
    }

    /// Configure a route for HTTP requests based on the HTTP `method` and
    /// URI `path`.  See the `HttpRouter` docs for information about how `path`
    /// is processed.  Requests matching `path` will be resolved to `handler`.
    pub fn insert(&mut self, endpoint: ApiEndpoint<Context>) {
        let method = endpoint.method.clone();
        let path = endpoint.path.clone();

        let all_segments = route_path_to_segments(path.as_str());

        let mut all_segments = all_segments.into_iter();
        let mut varnames: BTreeSet<String> = BTreeSet::new();

        let mut node: &mut Box<HttpRouterNode<Context>> = &mut self.root;
        while let Some(raw_segment) = all_segments.next() {
            let segment = PathSegment::from(raw_segment);

            node = match segment {
                PathSegment::Literal(lit) => {
                    // When inserting a literal we first check to see if a literal
                    // with the same segment exists. If it does we return it.
                    // If it doesn't we make a new entry for a literal.

                    let edge =
                        node.literal_edges.get_or_insert(BTreeMap::new());

                    edge.entry(lit)
                        .or_insert_with(|| Box::new(HttpRouterNode::new()))
                }
                PathSegment::VarnameSegment(new_varname) => {
                    insert_var(&path, &mut varnames, &new_varname);

                    let (varname, edge) = node.variable_edge.get_or_insert((
                        new_varname.clone(),
                        Box::new(HttpRouterNode::new()),
                    ));

                    if *new_varname != *varname {
                        // Don't allow people to use different names for
                        // the same part of the path.  Again, this could
                        // be supported, but it seems likely to be
                        // confusing and probably a mistake.
                        panic!(
                            "URI path \"{}\": attempted to use \
                             variable name \"{}\", but a different \
                             name (\"{}\") has already been used for \
                             this",
                            path, new_varname, varname
                        );
                    }

                    edge
                }
                PathSegment::VarnameWildcard(new_varname) => {
                    /*
                     * We don't accept further path segments after the .*.
                     */
                    if all_segments.next().is_some() {
                        panic!(
                            "URI path \"{}\": attempted to match segments \
                             after the wildcard variable \"{}\"",
                            path, new_varname,
                        );
                    }

                    insert_var(&path, &mut varnames, &new_varname);

                    let (varname, edge) = node.rest_edge.get_or_insert((
                        new_varname.clone(),
                        Box::new(HttpRouterNode::new()),
                    ));
                    if *new_varname != *varname {
                        /*
                         * Don't allow people to use different names for
                         * the same part of the path.  Again, this could
                         * be supported, but it seems likely to be
                         * confusing and probably a mistake.
                         */
                        panic!(
                            "URI path \"{}\": attempted to use \
                             variable name \"{}\", but a different \
                             name (\"{}\") has already been used for \
                             this",
                            path, new_varname, varname
                        );
                    }

                    edge
                }
            };
        }

        let methodname = method.as_str().to_uppercase();
        let existing_handlers =
            node.methods.entry(methodname.clone()).or_default();

        for handler in existing_handlers.iter() {
            if handler.versions.overlaps_with(&endpoint.versions) {
                if handler.versions == endpoint.versions {
                    panic!(
                        "URI path \"{}\": attempted to create duplicate route \
                        for method \"{}\"",
                        path, methodname
                    );
                } else {
                    panic!(
                        "URI path \"{}\": attempted to register multiple \
                        handlers for method \"{}\" with overlapping version \
                        ranges",
                        path, methodname
                    );
                }
            }
        }

        if endpoint.versions != ApiEndpointVersions::All {
            self.has_versioned_routes = true;
        }

        existing_handlers.push(endpoint);
    }

    /// Returns whether this router contains any routes that are constrained by
    /// version
    pub fn has_versioned_routes(&self) -> bool {
        self.has_versioned_routes
    }

    #[cfg(test)]
    pub fn lookup_route_unversioned(
        &self,
        method: &Method,
        path: InputPath<'_>,
    ) -> Result<RouterLookupResult<Context>, HttpError> {
        self.lookup_route(method, path, None)
    }

    /// Look up the route handler for an HTTP request having method `method` and
    /// URI path `path`.  A successful lookup produces a `RouterLookupResult`,
    /// which includes both the handler that can process this request and a map
    /// of variables assigned based on the request path as part of the lookup.
    /// On failure, this returns an `HttpError` appropriate for the failure
    /// mode.
    ///
    /// The matching rules here prioritize routes with more specificity
    /// over routes with less specificity (e.g /path/default is chosen before /path/{id}).
    ///
    /// A partial drawback of always picking the most specific route is that for two similar
    /// path structures, for example: `/foo/bar` and `/{id}/bar/hello` the path "/foo/bar/hello"
    /// will result in a 404, since the lookup logic will follow /foo/bar tree without consideration
    /// for the latter.
    pub fn lookup_route(
        &self,
        method: &Method,
        path: InputPath<'_>,
        version: Option<&Version>,
    ) -> Result<RouterLookupResult<Context>, HttpError> {
        let all_segments = input_path_to_segments(&path).map_err(|_| {
            HttpError::for_bad_request(
                None,
                String::from("invalid path encoding"),
            )
        })?;
        let mut all_segments = all_segments.into_iter();
        let mut node = &self.root;
        let mut variables = VariableSet::new();

        while let Some(segment) = all_segments.next() {
            let segment_string = segment.to_string();

            // First we check if the segment maps to a literal.
            if let Some(edges) = &node.literal_edges {
                if let Some(edge_node) = edges.get(&segment_string) {
                    node = edge_node;
                    continue;
                }
            };

            // Then we check if there is a valid variable edge.
            if let Some((varname, edge)) = &node.variable_edge {
                variables.insert(
                    varname.clone(),
                    VariableValue::String(segment_string),
                );
                node = &edge;
                continue;
            }

            // Lastly we check if there is a wildcard edge.
            if let Some((varname, edge)) = &node.rest_edge {
                let mut rest = vec![segment];
                while let Some(segment) = all_segments.next() {
                    rest.push(segment);
                }
                variables
                    .insert(varname.clone(), VariableValue::Components(rest));
                // There should be no outgoing edges since this is by
                // definition a terminal node
                assert!(edge.literal_edges.is_none());
                assert!(edge.variable_edge.is_none());
                assert!(edge.rest_edge.is_none());

                node = &edge;
                continue;
            }

            return Err(HttpError::for_not_found(
                None,
                String::from("no route found (no path in router)"),
            ));
        }

        // The wildcard match consumes the implicit, empty path segment
        if let Some((varname, edge)) = &node.rest_edge {
            variables
                .insert(varname.clone(), VariableValue::Components(vec![]));
            // There should be no outgoing edges
            assert!(edge.literal_edges.is_none());
            assert!(edge.variable_edge.is_none());
            assert!(edge.rest_edge.is_none());
            node = &edge;
        }

        // First, look for a matching implementation.
        let methodname = method.as_str().to_uppercase();
        if let Some(handler) = find_handler_matching_version(
            node.methods.get(&methodname).map(|v| v.as_slice()).unwrap_or(&[]),
            version,
        ) {
            return Ok(RouterLookupResult {
                handler: Arc::clone(&handler.handler),
                endpoint: RequestEndpointMetadata {
                    operation_id: handler.operation_id.clone(),
                    variables,
                    body_content_type: handler.body_content_type.clone(),
                    request_body_max_bytes: handler.request_body_max_bytes,
                },
            });
        }

        // We found no handler matching this path, method name, and version.
        // We're going to report a 404 ("Not Found") or 405 ("Method Not
        // Allowed").  It's a 405 if there are any handlers matching this path
        // and version for a different method.  It's a 404 otherwise.
        if node.methods.values().any(|handlers| {
            find_handler_matching_version(handlers, version).is_some()
        }) {
            let mut err = HttpError::for_client_error_with_status(
                None,
                ClientErrorStatusCode::METHOD_NOT_ALLOWED,
            );

            // Add `Allow` headers for the methods that *are* acceptable for
            // this path, as specified in § 15.5.0 RFC9110, which states:
            //
            // > The origin server MUST generate an Allow header field in a
            // > 405 response containing a list of the target resource's
            // > currently supported methods.
            //
            // See: https://httpwg.org/specs/rfc9110.html#status.405
            if let Some(hdrs) = err.headers.as_deref_mut() {
                hdrs.reserve(node.methods.len());
            }
            for allowed in node.methods.keys() {
                err.add_header(http::header::ALLOW, allowed)
                    .expect("method should be a valid allow header");
            }
            Err(err)
        } else {
            Err(HttpError::for_not_found(
                None,
                format!(
                    "route has no handlers for version {}",
                    match version {
                        Some(v) => v.to_string(),
                        None => String::from("<none>"),
                    }
                ),
            ))
        }
    }

    pub fn endpoints<'a>(
        &'a self,
        version: Option<&'a Version>,
    ) -> HttpRouterIter<'a, Context> {
        HttpRouterIter::new(self, version)
    }
}

/// Given a list of handlers, return the first one matching the given semver
///
/// If `version` is `None`, any handler will do.
fn find_handler_matching_version<'a, I, C>(
    handlers: I,
    version: Option<&Version>,
) -> Option<&'a ApiEndpoint<C>>
where
    I: IntoIterator<Item = &'a ApiEndpoint<C>>,
    C: ServerContext,
{
    handlers.into_iter().find(|h| h.versions.matches(version))
}

/// Insert a variable into the set after checking for duplicates.
fn insert_var(
    path: &str,
    varnames: &mut BTreeSet<String>,
    new_varname: &String,
) -> () {
    // Do not allow the same variable name to be used more than
    // once in the path.  Again, this could be supported (with
    // some caveats), but it seems more likely to be a mistake.
    if varnames.contains(new_varname) {
        panic!(
            "URI path \"{}\": variable name \"{}\" is used more than once",
            path, new_varname
        );
    }
    varnames.insert(new_varname.clone());
}

/// Route Interator implementation. We perform a preorder, depth first traversal
/// of the tree starting from the root node. For each node, we enumerate the
/// methods and then descend into its children (or single child in the case of
/// path parameter variables). `method` holds the iterator over the current
/// node's `methods`; `path` is a stack that represents the current collection
/// of path segments and the iterators at each corresponding node. We start with
/// the root node's `methods` iterator and a stack consisting of a
/// blank string and an iterator over the root node's children.
pub struct HttpRouterIter<'a, Context: ServerContext> {
    method:
        Box<dyn Iterator<Item = (&'a String, &'a ApiEndpoint<Context>)> + 'a>,
    path: Vec<(PathSegment, Box<PathIter<'a, Context>>)>,
    version: Option<&'a Version>,
}
type PathIter<'a, Context> =
    dyn Iterator<Item = (PathSegment, &'a Box<HttpRouterNode<Context>>)> + 'a;

fn iter_handlers_from_node<'a, 'b, 'c, C: ServerContext>(
    node: &'a HttpRouterNode<C>,
    version: Option<&'b Version>,
) -> Box<dyn Iterator<Item = (&'a String, &'a ApiEndpoint<C>)> + 'c>
where
    'a: 'c,
    'b: 'c,
{
    Box::new(node.methods.iter().flat_map(move |(m, handlers)| {
        handlers.iter().filter_map(move |h| {
            if h.versions.matches(version) {
                Some((m, h))
            } else {
                None
            }
        })
    }))
}

impl<'a, Context: ServerContext> HttpRouterIter<'a, Context> {
    fn new(
        router: &'a HttpRouter<Context>,
        version: Option<&'a Version>,
    ) -> Self {
        HttpRouterIter {
            method: iter_handlers_from_node(&router.root, version),
            path: vec![(
                PathSegment::Literal("".to_string()),
                HttpRouterIter::iter_node(&router.root),
            )],
            version,
        }
    }

    /// Produce an iterator over `node`'s children. This is the null (empty)
    /// iterator if there are no children, a single (once) iterator for a
    /// path parameter variable, and a modified iterator in the case of
    /// literal, explicit path segments.
    fn iter_node(
        node: &'a HttpRouterNode<Context>,
    ) -> Box<PathIter<'a, Context>> {
        let literal_iter = node.literal_edges.as_ref().map_or(
            Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _>>,
            |literals| {
                Box::new(literals.iter().map(move |(s, node)| {
                    (PathSegment::Literal(s.clone()), node)
                }))
            },
        );

        let variable_iter = node.variable_edge.as_ref().map_or(
            Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _>>,
            |(varname, node)| {
                Box::new(std::iter::once((
                    PathSegment::VarnameSegment(varname.clone()),
                    node,
                )))
            },
        );

        let rest_iter = node.rest_edge.as_ref().map_or(
            Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _>>,
            |(varname, node)| {
                Box::new(std::iter::once((
                    PathSegment::VarnameSegment(varname.clone()),
                    node,
                )))
            },
        );

        Box::new(literal_iter.chain(variable_iter).chain(rest_iter))
    }

    /// Produce a human-readable path from the current vector of path segments.
    fn path(&self) -> String {
        // Ignore the leading element as that's just a placeholder.
        let components: Vec<String> = self.path[1..]
            .iter()
            .map(|(c, _)| match c {
                PathSegment::Literal(s) => s.clone(),
                PathSegment::VarnameSegment(s) => format!("{{{}}}", s),
                PathSegment::VarnameWildcard(s) => format!("{{{}:.*}}", s),
            })
            .collect();

        // Prepend "/" to the "/"-delimited path.
        format!("/{}", components.join("/"))
    }
}

impl<'a, Context: ServerContext> Iterator for HttpRouterIter<'a, Context> {
    type Item = (String, String, &'a ApiEndpoint<Context>);

    fn next(&mut self) -> Option<Self::Item> {
        // If there are no path components left then we've reached the end of
        // our traversal. Making this case explicit isn't strictly required,
        // but is added for clarity.
        if self.path.is_empty() {
            return None;
        }

        loop {
            match self.method.next() {
                Some((m, ref e)) => break Some((self.path(), m.clone(), e)),
                None => {
                    // We've iterated fully through the method in this node so
                    // it's time to find the next node.
                    match self.path.last_mut() {
                        None => break None,
                        Some((_, ref mut last)) => match last.next() {
                            None => {
                                self.path.pop();
                                assert!(self.method.next().is_none());
                            }
                            Some((path_component, node)) => {
                                self.path.push((
                                    path_component,
                                    HttpRouterIter::iter_node(node),
                                ));
                                self.method = iter_handlers_from_node(
                                    &node,
                                    self.version,
                                );
                            }
                        },
                    }
                }
            }
        }
    }
}

/// Helper function for taking a Uri path and producing a `Vec<String>` of
/// URL-decoded strings, each representing one segment of the path. The input is
/// percent-encoded. Empty segments i.e. due to consecutive "/" characters or a
/// leading "/" are omitted.
///
/// Regarding "dot-segments" ("." and ".."), RFC 3986 section 3.3 says this:
///    The path segments "." and "..", also known as dot-segments, are
///    defined for relative reference within the path name hierarchy.  They
///    are intended for use at the beginning of a relative-path reference
///    (Section 4.2) to indicate relative position within the hierarchical
///    tree of names.  This is similar to their role within some operating
///    systems' file directory structures to indicate the current directory
///    and parent directory, respectively.  However, unlike in a file
///    system, these dot-segments are only interpreted within the URI path
///    hierarchy and are removed as part of the resolution process (Section
///    5.2).
///
/// While nothing prohibits APIs from including dot-segments. We see no strong
/// case for allowing them in paths, and plenty of pitfalls if we were to
/// require consumers to consider them (e.g. "GET /../../../etc/passwd"). Note
/// that consumers may be susceptible to other information leaks, for example
/// if a client were able to follow a symlink to the root of the filesystem. As
/// always, it is incumbent on the consumer and *critical* to validate input.
fn input_path_to_segments(path: &InputPath) -> Result<Vec<String>, String> {
    // We're given the "path" portion of a URI and we want to construct an
    // array of the segments of the path.   Relevant references:
    //
    //    RFC 7230 HTTP/1.1 Syntax and Routing
    //             (particularly: 2.7.3 on normalization)
    //    RFC 3986 Uniform Resource Identifier (URI): Generic Syntax
    //             (particularly: 6.2.2 on comparison)
    //
    // TODO-hardening We should revisit this.  We want to consider a couple of
    // things:
    // - what it means (and what we should do) if the path does not begin with
    //   a leading "/"
    // - how to handle paths that end in "/" (in some cases, ought this send a
    //   300-level redirect?)
    //
    // It would seem obvious to reach for the Rust "url" crate. That crate
    // parses complete URLs, which include a scheme and authority section that
    // does not apply here. We could certainly make one up (e.g.,
    // "http://127.0.0.1") and construct a URL whose path matches the path we
    // were given. However, while it seems natural that our internal
    // representation would not be percent-encoded, the "url" crate
    // percent-encodes any path that it's given. Further, we probably want to
    // treat consecutive "/" characters as equivalent to a single "/", but that
    // crate treats them separately (which is not unreasonable, since it's not
    // clear that the above RFCs say anything about whether empty segments
    // should be ignored). The net result is that that crate doesn't buy us
    // much here, but it does create more work, so we'll just split it
    // ourselves.
    path.0
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| match segment {
            "." | ".." => Err("dot-segments are not permitted".to_string()),
            _ => Ok(percent_decode_str(segment)
                .decode_utf8()
                .map_err(|e| e.to_string())?
                .to_string()),
        })
        .collect()
}

/// Whereas in `input_path_to_segments()` we must accommodate any user input, when
/// processing paths specified by the client program we can be more stringent and
/// fail via a panic! rather than an error. We do not percent-decode the path
/// meaning that programs may specify path segments that would require
/// percent-encoding by clients. Paths *must* begin with a "/"; only the final
/// segment may be empty i.e. the path may end with a "/".
pub fn route_path_to_segments(path: &str) -> Vec<&str> {
    if !matches!(path.chars().next(), Some('/')) {
        panic!("route paths must begin with a '/': '{}'", path);
    }
    let mut ret = path.split('/').skip(1).collect::<Vec<_>>();
    for segment in &ret[..ret.len() - 1] {
        if segment.is_empty() {
            panic!("path segments may not be empty: '{}'", path);
        }
    }

    // TODO pop off the last element if it's empty; today we treat a trailing
    // "/" as identical to a path without a trailing "/", but we may want to
    // support the distinction.
    if ret[ret.len() - 1] == "" {
        ret.pop();
    }
    ret
}

#[cfg(test)]
mod test {
    use super::super::error::HttpError;
    use super::super::handler::HttpRouteHandler;
    use super::super::handler::RequestContext;
    use super::super::handler::RouteHandler;
    use super::input_path_to_segments;
    use super::HttpRouter;
    use super::PathSegment;
    use crate::api_description::ApiEndpointBodyContentType;
    use crate::api_description::ApiEndpointVersions;
    use crate::from_map::from_map;
    use crate::router::VariableValue;
    use crate::ApiEndpoint;
    use crate::ApiEndpointResponse;
    use crate::Body;
    use http::Method;
    use http::StatusCode;
    use hyper::Response;
    use semver::Version;
    use serde::Deserialize;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    async fn test_handler(
        _: RequestContext<()>,
    ) -> Result<Response<Body>, HttpError> {
        panic!("test handler is not supposed to run");
    }

    fn new_handler() -> Arc<dyn RouteHandler<()>> {
        HttpRouteHandler::new(test_handler)
    }

    fn new_handler_named(name: &str) -> Arc<dyn RouteHandler<()>> {
        HttpRouteHandler::new_with_name(test_handler, name)
    }

    fn new_endpoint(
        handler: Arc<dyn RouteHandler<()>>,
        method: Method,
        path: &str,
    ) -> ApiEndpoint<()> {
        new_endpoint_versions(handler, method, path, ApiEndpointVersions::All)
    }

    fn new_endpoint_versions(
        handler: Arc<dyn RouteHandler<()>>,
        method: Method,
        path: &str,
        versions: ApiEndpointVersions,
    ) -> ApiEndpoint<()> {
        ApiEndpoint {
            operation_id: "test_handler".to_string(),
            handler,
            method,
            path: path.to_string(),
            parameters: vec![],
            body_content_type: ApiEndpointBodyContentType::default(),
            request_body_max_bytes: None,
            response: ApiEndpointResponse::default(),
            error: None,
            summary: None,
            description: None,
            tags: vec![],
            extension_mode: Default::default(),
            visible: true,
            deprecated: false,
            versions,
        }
    }

    #[test]
    #[should_panic(
        expected = "HTTP URI path segment variable name must not be empty"
    )]
    fn test_variable_name_empty() {
        let mut router = HttpRouter::new();
        router.insert(new_endpoint(new_handler(), Method::GET, "/foo/{}"));
    }

    #[test]
    #[should_panic(
        expected = "HTTP URI path segment variable missing trailing \"}\""
    )]
    fn test_variable_name_bad_end() {
        let mut router = HttpRouter::new();
        router.insert(new_endpoint(
            new_handler(),
            Method::GET,
            "/foo/{asdf/foo",
        ));
    }

    #[test]
    #[should_panic(
        expected = "HTTP URI path segment variable missing leading \"{\""
    )]
    fn test_variable_name_bad_start() {
        let mut router = HttpRouter::new();
        router.insert(new_endpoint(
            new_handler(),
            Method::GET,
            "/foo/asdf}/foo",
        ));
    }

    #[test]
    #[should_panic(expected = "URI path \"/boo\": attempted to create \
                               duplicate route for method \"GET\"")]
    fn test_duplicate_route1() {
        let mut router = HttpRouter::new();
        router.insert(new_endpoint(new_handler(), Method::GET, "/boo"));
        router.insert(new_endpoint(new_handler(), Method::GET, "/boo"));
    }

    #[test]
    #[should_panic(expected = "URI path \"/foo/bar/\": attempted to create \
                               duplicate route for method \"GET\"")]
    fn test_duplicate_route2() {
        let mut router = HttpRouter::new();
        router.insert(new_endpoint(new_handler(), Method::GET, "/foo/bar"));
        router.insert(new_endpoint(new_handler(), Method::GET, "/foo/bar/"));
    }

    #[test]
    #[should_panic(expected = "path segments may not be empty: '//'")]
    fn test_duplicate_route3() {
        let mut router = HttpRouter::new();
        router.insert(new_endpoint(new_handler(), Method::GET, "/"));
        router.insert(new_endpoint(new_handler(), Method::GET, "//"));
    }

    #[test]
    #[should_panic(expected = "URI path \"/boo\": attempted to create \
                               duplicate route for method \"GET\"")]
    fn test_duplicate_route_same_version() {
        let mut router = HttpRouter::new();
        router.insert(new_endpoint_versions(
            new_handler(),
            Method::GET,
            "/boo",
            ApiEndpointVersions::From(Version::new(1, 2, 3)),
        ));
        router.insert(new_endpoint_versions(
            new_handler(),
            Method::GET,
            "/boo",
            ApiEndpointVersions::From(Version::new(1, 2, 3)),
        ));
    }

    #[test]
    #[should_panic(expected = "URI path \"/boo\": attempted to register \
                               multiple handlers for method \"GET\" with \
                               overlapping version ranges")]
    fn test_duplicate_route_overlapping_version() {
        let mut router = HttpRouter::new();
        router.insert(new_endpoint_versions(
            new_handler(),
            Method::GET,
            "/boo",
            ApiEndpointVersions::From(Version::new(1, 2, 3)),
        ));
        router.insert(new_endpoint_versions(
            new_handler(),
            Method::GET,
            "/boo",
            ApiEndpointVersions::From(Version::new(4, 5, 6)),
        ));
    }

    #[test]
    #[should_panic(expected = "URI path \"/projects/{id}/insts/{id}\": \
                               variable name \"id\" is used more than once")]
    fn test_duplicate_varname() {
        let mut router = HttpRouter::new();
        router.insert(new_endpoint(
            new_handler(),
            Method::GET,
            "/projects/{id}/insts/{id}",
        ));
    }

    #[test]
    #[should_panic(expected = "URI path \"/projects/{id}\": attempted to use \
                               variable name \"id\", but a different name \
                               (\"project_id\") has already been used for \
                               this")]
    fn test_inconsistent_varname() {
        let mut router = HttpRouter::new();
        router.insert(new_endpoint(
            new_handler(),
            Method::GET,
            "/projects/{project_id}",
        ));
        router.insert(new_endpoint(
            new_handler(),
            Method::GET,
            "/projects/{id}",
        ));
    }

    #[test]
    fn test_variable_after_literal() {
        let mut router = HttpRouter::new();
        router.insert(new_endpoint(
            new_handler(),
            Method::GET,
            "/projects/default",
        ));
        router.insert(new_endpoint(
            new_handler(),
            Method::GET,
            "/projects/{id}",
        ));
    }

    #[test]
    fn test_more_specific_route_wins() {
        let mut router = HttpRouter::new();
        router.insert(new_endpoint(
            new_handler_named("route_one"),
            Method::GET,
            "/projects/{id}",
        ));
        router.insert(new_endpoint(
            new_handler_named("route_two"),
            Method::GET,
            "/projects/default",
        ));
        let result = router
            .lookup_route_unversioned(&Method::GET, "/projects/default".into())
            .unwrap();
        assert_eq!(result.handler.label(), "route_two");
    }

    #[test]
    fn test_less_specific_route_still_accessible() {
        let mut router = HttpRouter::new();
        router.insert(new_endpoint(
            new_handler_named("route_one"),
            Method::GET,
            "/projects/{id}",
        ));
        router.insert(new_endpoint(
            new_handler_named("route_two"),
            Method::GET,
            "/projects/default",
        ));
        let result = router
            .lookup_route_unversioned(&Method::GET, "/projects/lol".into())
            .unwrap();
        assert_eq!(result.handler.label(), "route_one");
    }

    #[test]
    fn test_catch_all_routes_work() {
        let mut router = HttpRouter::new();
        router.insert(new_endpoint(
            new_handler_named("route_one"),
            Method::GET,
            "/projects/{id}",
        ));
        router.insert(new_endpoint(
            new_handler_named("route_two"),
            Method::GET,
            "/projects/default",
        ));
        router.insert(new_endpoint(
            new_handler_named("route_three"),
            Method::GET,
            "/{path:.*}",
        ));
        let result = router
            .lookup_route_unversioned(&Method::GET, "/projects/lol".into())
            .unwrap();
        assert_eq!(result.handler.label(), "route_one");
        let result = router
            .lookup_route_unversioned(&Method::GET, "/projects/default".into())
            .unwrap();
        assert_eq!(result.handler.label(), "route_two");
        let result = router
            .lookup_route_unversioned(&Method::GET, "/lolwut".into())
            .unwrap();
        assert_eq!(result.handler.label(), "route_three");

        let result = router
            .lookup_route_unversioned(&Method::GET, "/lolwut/test".into())
            .unwrap();
        assert_eq!(result.handler.label(), "route_three");

        let result = router
            .lookup_route_unversioned(&Method::GET, "/lolwut".into())
            .unwrap();
        assert_eq!(result.handler.label(), "route_three");
    }

    #[test]
    fn test_no_backmatching() {
        // if the indicated route starts with a literal that exists we don't
        // go back and match on variable strings even if the route exists.
        // For example, for a router with the routes `/projects/default` and `/{id}/default/lol`,
        // If the path "/projects/default/lol" comes in it will be a 404 since the first segment
        // already matched with the `projects` literal.
        // TODO():We can probably solve for this but, not worth the time right now.
        let mut router = HttpRouter::new();
        router.insert(new_endpoint(
            new_handler_named("route_one"),
            Method::GET,
            "/projects/default",
        ));
        router.insert(new_endpoint(
            new_handler_named("route_two"),
            Method::GET,
            "/{id}/default/lol",
        ));
        // Access to the more specific route works
        let result = router
            .lookup_route_unversioned(&Method::GET, "/projects/default".into())
            .unwrap();
        assert_eq!(result.handler.label(), "route_one");

        // Access to /projects/ starts down the /projects path and therefore doesnt' match
        assert!(router
            .lookup_route_unversioned(
                &Method::GET,
                "/projects/default/lol".into()
            )
            .is_err());

        // Access to the less specific path as long as it's not /projects works.
        let result = router
            .lookup_route_unversioned(
                &Method::GET,
                "/some_id/default/lol".into(),
            )
            .unwrap();
        assert_eq!(result.handler.label(), "route_two");
    }

    #[test]
    fn test_literal_after_variable() {
        let mut router = HttpRouter::new();
        router.insert(new_endpoint(
            new_handler(),
            Method::GET,
            "/projects/{id}",
        ));
        router.insert(new_endpoint(
            new_handler(),
            Method::GET,
            "/projects/default",
        ));
    }

    #[test]
    fn test_literal_after_regex() {
        let mut router = HttpRouter::new();
        router.insert(new_endpoint(
            new_handler(),
            Method::GET,
            "/projects/{rest:.*}",
        ));
        router.insert(new_endpoint(
            new_handler(),
            Method::GET,
            "/projects/default",
        ));
    }

    #[test]
    #[should_panic(expected = "Only the pattern '.*' is currently supported")]
    fn test_bogus_regex() {
        let mut router = HttpRouter::new();
        router.insert(new_endpoint(
            new_handler(),
            Method::GET,
            "/word/{rest:[a-z]*}",
        ));
    }

    #[test]
    #[should_panic(expected = "URI path \"/some/{more:.*}/{stuff}\": \
                               attempted to match segments after the \
                               wildcard variable \"more\"")]
    fn test_more_after_regex() {
        let mut router = HttpRouter::new();
        router.insert(new_endpoint(
            new_handler(),
            Method::GET,
            "/some/{more:.*}/{stuff}",
        ));
    }

    // TODO: We allow a trailing slash after the wildcard specifier, but we may
    // reconsider this if we decided to distinguish between the presence or
    // absence of the trailing slash.
    #[test]
    fn test_slash_after_wildcard_is_fine_dot_dot_dot_for_now() {
        let mut router = HttpRouter::new();
        router.insert(new_endpoint(
            new_handler(),
            Method::GET,
            "/some/{more:.*}/",
        ));
    }

    #[test]
    fn test_error_cases() {
        let mut router = HttpRouter::new();

        // Check a few initial conditions.
        let error = router
            .lookup_route_unversioned(&Method::GET, "/".into())
            .unwrap_err();
        assert_eq!(error.status_code, StatusCode::NOT_FOUND);
        let error = router
            .lookup_route_unversioned(&Method::GET, "////".into())
            .unwrap_err();
        assert_eq!(error.status_code, StatusCode::NOT_FOUND);
        let error = router
            .lookup_route_unversioned(&Method::GET, "/foo/bar".into())
            .unwrap_err();
        assert_eq!(error.status_code, StatusCode::NOT_FOUND);
        let error = router
            .lookup_route_unversioned(&Method::GET, "//foo///bar".into())
            .unwrap_err();
        assert_eq!(error.status_code, StatusCode::NOT_FOUND);

        // Insert a route into the middle of the tree.  This will let us look at
        // parent nodes, sibling nodes, and child nodes.
        router.insert(new_endpoint(new_handler(), Method::GET, "/foo/bar"));
        assert!(router
            .lookup_route_unversioned(&Method::GET, "/foo/bar".into())
            .is_ok());
        assert!(router
            .lookup_route_unversioned(&Method::GET, "/foo/bar/".into())
            .is_ok());
        assert!(router
            .lookup_route_unversioned(&Method::GET, "//foo/bar".into())
            .is_ok());
        assert!(router
            .lookup_route_unversioned(&Method::GET, "//foo//bar".into())
            .is_ok());
        assert!(router
            .lookup_route_unversioned(&Method::GET, "//foo//bar//".into())
            .is_ok());
        assert!(router
            .lookup_route_unversioned(&Method::GET, "///foo///bar///".into())
            .is_ok());

        // TODO-cleanup: consider having a "build" step that constructs a
        // read-only router and does validation like making sure that there's a
        // GET route on all nodes?
        let error = router
            .lookup_route_unversioned(&Method::GET, "/".into())
            .unwrap_err();
        assert_eq!(error.status_code, StatusCode::NOT_FOUND);
        let error = router
            .lookup_route_unversioned(&Method::GET, "/foo".into())
            .unwrap_err();
        assert_eq!(error.status_code, StatusCode::NOT_FOUND);
        let error = router
            .lookup_route_unversioned(&Method::GET, "//foo".into())
            .unwrap_err();
        assert_eq!(error.status_code, StatusCode::NOT_FOUND);
        let error = router
            .lookup_route_unversioned(&Method::GET, "/foo/bar/baz".into())
            .unwrap_err();
        assert_eq!(error.status_code, StatusCode::NOT_FOUND);

        let error = router
            .lookup_route_unversioned(&Method::PUT, "/foo/bar".into())
            .unwrap_err();
        assert_eq!(error.status_code, StatusCode::METHOD_NOT_ALLOWED);
        let error = router
            .lookup_route_unversioned(&Method::PUT, "/foo/bar/".into())
            .unwrap_err();
        assert_eq!(error.status_code, StatusCode::METHOD_NOT_ALLOWED);

        // Check error cases that are specific (or handled differently) when
        // routes are versioned.
        let mut router = HttpRouter::new();
        router.insert(new_endpoint_versions(
            new_handler(),
            Method::GET,
            "/foo",
            ApiEndpointVersions::from(Version::new(1, 0, 0)),
        ));
        let error = router
            .lookup_route(
                &Method::GET,
                "/foo".into(),
                Some(&Version::new(0, 9, 0)),
            )
            .unwrap_err();
        assert_eq!(error.status_code, StatusCode::NOT_FOUND);
        let error = router
            .lookup_route(
                &Method::PUT,
                "/foo".into(),
                Some(&Version::new(1, 1, 0)),
            )
            .unwrap_err();
        assert_eq!(error.status_code, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn test_router_basic() {
        let mut router = HttpRouter::new();

        // Insert a handler at the root and verify that we get that handler
        // back, even if we use different names that normalize to "/".
        // Before we start, sanity-check that there's nothing at the root
        // already.  Other test cases examine the errors in more detail.
        assert!(router
            .lookup_route_unversioned(&Method::GET, "/".into())
            .is_err());
        router.insert(new_endpoint(new_handler_named("h1"), Method::GET, "/"));
        let result =
            router.lookup_route_unversioned(&Method::GET, "/".into()).unwrap();
        assert_eq!(result.handler.label(), "h1");
        assert!(result.endpoint.variables.is_empty());
        let result =
            router.lookup_route_unversioned(&Method::GET, "//".into()).unwrap();
        assert_eq!(result.handler.label(), "h1");
        assert!(result.endpoint.variables.is_empty());
        let result = router
            .lookup_route_unversioned(&Method::GET, "///".into())
            .unwrap();
        assert_eq!(result.handler.label(), "h1");
        assert!(result.endpoint.variables.is_empty());

        // Now insert a handler for a different method at the root.  Verify that
        // we get both this handler and the previous one if we ask for the
        // corresponding method and that we get no handler for a different,
        // third method.
        assert!(router
            .lookup_route_unversioned(&Method::PUT, "/".into())
            .is_err());
        router.insert(new_endpoint(new_handler_named("h2"), Method::PUT, "/"));
        let result =
            router.lookup_route_unversioned(&Method::PUT, "/".into()).unwrap();
        assert_eq!(result.handler.label(), "h2");
        assert!(result.endpoint.variables.is_empty());
        let result =
            router.lookup_route_unversioned(&Method::GET, "/".into()).unwrap();
        assert_eq!(result.handler.label(), "h1");
        assert!(result.endpoint.variables.is_empty());
        assert!(router
            .lookup_route_unversioned(&Method::DELETE, "/".into())
            .is_err());

        // Now insert a handler one level deeper.  Verify that all the previous
        // handlers behave as we expect, and that we have one handler at the new
        // path, whichever name we use for it.
        assert!(router
            .lookup_route_unversioned(&Method::GET, "/foo".into())
            .is_err());
        router.insert(new_endpoint(
            new_handler_named("h3"),
            Method::GET,
            "/foo",
        ));
        let result =
            router.lookup_route_unversioned(&Method::PUT, "/".into()).unwrap();
        assert_eq!(result.handler.label(), "h2");
        assert!(result.endpoint.variables.is_empty());
        let result =
            router.lookup_route_unversioned(&Method::GET, "/".into()).unwrap();
        assert_eq!(result.handler.label(), "h1");
        assert!(result.endpoint.variables.is_empty());
        let result = router
            .lookup_route_unversioned(&Method::GET, "/foo".into())
            .unwrap();
        assert_eq!(result.handler.label(), "h3");
        assert!(result.endpoint.variables.is_empty());
        let result = router
            .lookup_route_unversioned(&Method::GET, "/foo/".into())
            .unwrap();
        assert_eq!(result.handler.label(), "h3");
        assert!(result.endpoint.variables.is_empty());
        let result = router
            .lookup_route_unversioned(&Method::GET, "//foo//".into())
            .unwrap();
        assert_eq!(result.handler.label(), "h3");
        assert!(result.endpoint.variables.is_empty());
        let result = router
            .lookup_route_unversioned(&Method::GET, "/foo//".into())
            .unwrap();
        assert_eq!(result.handler.label(), "h3");
        assert!(result.endpoint.variables.is_empty());
        assert!(router
            .lookup_route_unversioned(&Method::PUT, "/foo".into())
            .is_err());
        assert!(router
            .lookup_route_unversioned(&Method::PUT, "/foo/".into())
            .is_err());
        assert!(router
            .lookup_route_unversioned(&Method::PUT, "//foo//".into())
            .is_err());
        assert!(router
            .lookup_route_unversioned(&Method::PUT, "/foo//".into())
            .is_err());
    }

    #[test]
    fn test_router_versioned() {
        // Install handlers for a particular route for a bunch of different
        // versions.
        //
        // This is not exhaustive because the matching logic is tested
        // exhaustively elsewhere.
        let method = Method::GET;
        let path: super::InputPath<'static> = "/foo".into();
        let mut router = HttpRouter::new();
        router.insert(new_endpoint_versions(
            new_handler_named("h1"),
            method.clone(),
            "/foo",
            ApiEndpointVersions::until(Version::new(1, 2, 3)),
        ));
        router.insert(new_endpoint_versions(
            new_handler_named("h2"),
            method.clone(),
            "/foo",
            ApiEndpointVersions::from_until(
                Version::new(2, 0, 0),
                Version::new(3, 0, 0),
            )
            .unwrap(),
        ));
        router.insert(new_endpoint_versions(
            new_handler_named("h3"),
            method.clone(),
            "/foo",
            ApiEndpointVersions::From(Version::new(5, 0, 0)),
        ));

        // Check what happens for a representative range of versions.
        let result = router
            .lookup_route(&method, path, Some(&Version::new(1, 2, 2)))
            .unwrap();
        assert_eq!(result.handler.label(), "h1");
        let error = router
            .lookup_route(&method, path, Some(&Version::new(1, 2, 3)))
            .unwrap_err();
        assert_eq!(error.status_code, StatusCode::NOT_FOUND);

        let result = router
            .lookup_route(&method, path, Some(&Version::new(2, 0, 0)))
            .unwrap();
        assert_eq!(result.handler.label(), "h2");
        let result = router
            .lookup_route(&method, path, Some(&Version::new(2, 1, 0)))
            .unwrap();
        assert_eq!(result.handler.label(), "h2");

        let error = router
            .lookup_route(&method, path, Some(&Version::new(3, 0, 0)))
            .unwrap_err();
        assert_eq!(error.status_code, StatusCode::NOT_FOUND);
        let error = router
            .lookup_route(&method, path, Some(&Version::new(3, 0, 1)))
            .unwrap_err();
        assert_eq!(error.status_code, StatusCode::NOT_FOUND);

        let error = router
            .lookup_route(&method, path, Some(&Version::new(4, 99, 99)))
            .unwrap_err();
        assert_eq!(error.status_code, StatusCode::NOT_FOUND);

        let result = router
            .lookup_route(&method, path, Some(&Version::new(5, 0, 0)))
            .unwrap();
        assert_eq!(result.handler.label(), "h3");
        let result = router
            .lookup_route(&method, path, Some(&Version::new(128313, 0, 0)))
            .unwrap();
        assert_eq!(result.handler.label(), "h3");
    }

    #[test]
    fn test_embedded_non_variable() {
        // This isn't an important use case today, but we'd like to know if we
        // change the behavior, intentionally or otherwise.
        let mut router = HttpRouter::new();
        assert!(router
            .lookup_route_unversioned(&Method::GET, "/not{a}variable".into())
            .is_err());
        router.insert(new_endpoint(
            new_handler_named("h4"),
            Method::GET,
            "/not{a}variable",
        ));
        let result = router
            .lookup_route_unversioned(&Method::GET, "/not{a}variable".into())
            .unwrap();
        assert_eq!(result.handler.label(), "h4");
        assert!(result.endpoint.variables.is_empty());
        assert!(router
            .lookup_route_unversioned(&Method::GET, "/not{b}variable".into())
            .is_err());
        assert!(router
            .lookup_route_unversioned(&Method::GET, "/notnotavariable".into())
            .is_err());
    }

    #[test]
    fn test_variables_basic() {
        // Basic test using a variable.
        let mut router = HttpRouter::new();
        router.insert(new_endpoint(
            new_handler_named("h5"),
            Method::GET,
            "/projects/{project_id}",
        ));
        assert!(router
            .lookup_route_unversioned(&Method::GET, "/projects".into())
            .is_err());
        assert!(router
            .lookup_route_unversioned(&Method::GET, "/projects/".into())
            .is_err());
        let result = router
            .lookup_route_unversioned(&Method::GET, "/projects/p12345".into())
            .unwrap();
        assert_eq!(result.handler.label(), "h5");
        assert_eq!(
            result.endpoint.variables.keys().collect::<Vec<&String>>(),
            vec!["project_id"]
        );
        assert_eq!(
            *result.endpoint.variables.get("project_id").unwrap(),
            VariableValue::String("p12345".to_string())
        );
        assert!(router
            .lookup_route_unversioned(
                &Method::GET,
                "/projects/p12345/child".into()
            )
            .is_err());
        let result = router
            .lookup_route_unversioned(&Method::GET, "/projects/p12345/".into())
            .unwrap();
        assert_eq!(result.handler.label(), "h5");
        assert_eq!(
            *result.endpoint.variables.get("project_id").unwrap(),
            VariableValue::String("p12345".to_string())
        );
        let result = router
            .lookup_route_unversioned(
                &Method::GET,
                "/projects///p12345//".into(),
            )
            .unwrap();
        assert_eq!(result.handler.label(), "h5");
        assert_eq!(
            *result.endpoint.variables.get("project_id").unwrap(),
            VariableValue::String("p12345".to_string())
        );
        // Trick question!
        let result = router
            .lookup_route_unversioned(
                &Method::GET,
                "/projects/{project_id}".into(),
            )
            .unwrap();
        assert_eq!(result.handler.label(), "h5");
        assert_eq!(
            *result.endpoint.variables.get("project_id").unwrap(),
            VariableValue::String("{project_id}".to_string())
        );
    }

    #[test]
    fn test_variables_multi() {
        // Exercise a case with multiple variables.
        let mut router = HttpRouter::new();
        router.insert(new_endpoint(
            new_handler_named("h6"),
            Method::GET,
            "/projects/{project_id}/instances/{instance_id}/fwrules/\
             {fwrule_id}/info",
        ));
        let result = router
            .lookup_route_unversioned(
                &Method::GET,
                "/projects/p1/instances/i2/fwrules/fw3/info".into(),
            )
            .unwrap();
        assert_eq!(result.handler.label(), "h6");
        assert_eq!(
            result.endpoint.variables.keys().collect::<Vec<&String>>(),
            vec!["fwrule_id", "instance_id", "project_id"]
        );
        assert_eq!(
            *result.endpoint.variables.get("project_id").unwrap(),
            VariableValue::String("p1".to_string())
        );
        assert_eq!(
            *result.endpoint.variables.get("instance_id").unwrap(),
            VariableValue::String("i2".to_string())
        );
        assert_eq!(
            *result.endpoint.variables.get("fwrule_id").unwrap(),
            VariableValue::String("fw3".to_string())
        );
    }

    #[test]
    fn test_empty_variable() {
        // Exercise a case where a broken implementation might erroneously
        // assign a variable to the empty string.
        let mut router = HttpRouter::new();
        router.insert(new_endpoint(
            new_handler_named("h7"),
            Method::GET,
            "/projects/{project_id}/instances",
        ));
        assert!(router
            .lookup_route_unversioned(
                &Method::GET,
                "/projects/instances".into()
            )
            .is_err());
        assert!(router
            .lookup_route_unversioned(
                &Method::GET,
                "/projects//instances".into()
            )
            .is_err());
        assert!(router
            .lookup_route_unversioned(
                &Method::GET,
                "/projects///instances".into()
            )
            .is_err());
        let result = router
            .lookup_route_unversioned(
                &Method::GET,
                "/projects/foo/instances".into(),
            )
            .unwrap();
        assert_eq!(result.handler.label(), "h7");
    }

    #[test]
    fn test_variables_glob() {
        let mut router = HttpRouter::new();
        router.insert(new_endpoint(
            new_handler_named("h8"),
            Method::OPTIONS,
            "/console/{path:.*}",
        ));

        let result = router
            .lookup_route_unversioned(
                &Method::OPTIONS,
                "/console/missiles/launch".into(),
            )
            .unwrap();

        assert_eq!(
            result.endpoint.variables.get("path"),
            Some(&VariableValue::Components(vec![
                "missiles".to_string(),
                "launch".to_string()
            ]))
        );
    }

    #[test]
    fn test_variable_rename() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct MyPath {
            #[serde(rename = "type")]
            t: String,
            #[serde(rename = "ref")]
            r: String,
            #[serde(rename = "@")]
            at: String,
        }

        let mut router = HttpRouter::new();
        router.insert(new_endpoint(
            new_handler_named("h8"),
            Method::OPTIONS,
            "/{type}/{ref}/{@}",
        ));

        let result = router
            .lookup_route_unversioned(
                &Method::OPTIONS,
                "/console/missiles/launch".into(),
            )
            .unwrap();

        let path =
            from_map::<MyPath, VariableValue>(&result.endpoint.variables)
                .unwrap();

        assert_eq!(path.t, "console");
        assert_eq!(path.r, "missiles");
        assert_eq!(path.at, "launch");
    }

    #[test]
    fn test_iter_null() {
        let router = HttpRouter::<()>::new();
        let ret: Vec<_> = router.endpoints(None).map(|x| (x.0, x.1)).collect();
        assert_eq!(ret, vec![]);
    }

    #[test]
    fn test_iter() {
        let mut router = HttpRouter::new();
        router.insert(new_endpoint(
            new_handler_named("root"),
            Method::GET,
            "/",
        ));
        router.insert(new_endpoint(
            new_handler_named("i"),
            Method::GET,
            "/projects/{project_id}/instances",
        ));
        let ret: Vec<_> = router.endpoints(None).map(|x| (x.0, x.1)).collect();
        assert_eq!(
            ret,
            vec![
                ("/".to_string(), "GET".to_string(),),
                (
                    "/projects/{project_id}/instances".to_string(),
                    "GET".to_string(),
                ),
            ]
        );
    }

    #[test]
    fn test_iter2() {
        let mut router = HttpRouter::new();
        router.insert(new_endpoint(
            new_handler_named("root_get"),
            Method::GET,
            "/",
        ));
        router.insert(new_endpoint(
            new_handler_named("root_post"),
            Method::POST,
            "/",
        ));
        let ret: Vec<_> = router.endpoints(None).map(|x| (x.0, x.1)).collect();
        assert_eq!(
            ret,
            vec![
                ("/".to_string(), "GET".to_string(),),
                ("/".to_string(), "POST".to_string(),),
            ]
        );
    }

    #[test]
    fn test_segments() {
        let segs =
            input_path_to_segments(&"//foo/bar/baz%2fbuzz".into()).unwrap();
        assert_eq!(segs, vec!["foo", "bar", "baz/buzz"]);
    }

    #[test]
    fn test_path_segment() {
        let seg = PathSegment::from("abc");
        assert_eq!(seg, PathSegment::Literal("abc".to_string()));

        let seg = PathSegment::from("{words}");
        assert_eq!(seg, PathSegment::VarnameSegment("words".to_string()));

        let seg = PathSegment::from("{rest:.*}");
        assert_eq!(seg, PathSegment::VarnameWildcard("rest".to_string()),);
    }

    #[test]
    #[should_panic]
    fn test_bad_path_segment1() {
        let _ = PathSegment::from("{foo");
    }

    #[test]
    #[should_panic]
    fn test_bad_path_segment2() {
        let _ = PathSegment::from("bar}");
    }

    #[test]
    #[should_panic]
    fn test_bad_path_segment3() {
        let _ = PathSegment::from("{}");
    }

    #[test]
    #[should_panic]
    fn test_bad_path_segment4() {
        let _ = PathSegment::from("{varname:abc+}");
    }

    #[test]
    fn test_map() {
        #[derive(Deserialize)]
        struct A {
            bbb: String,
            ccc: Vec<String>,
        }

        let mut map = BTreeMap::new();
        map.insert(
            "bbb".to_string(),
            VariableValue::String("doggos".to_string()),
        );
        map.insert(
            "ccc".to_string(),
            VariableValue::Components(vec![
                "lizzie".to_string(),
                "brickley".to_string(),
            ]),
        );

        match from_map::<A, VariableValue>(&map) {
            Ok(a) => {
                assert_eq!(a.bbb, "doggos");
                assert_eq!(a.ccc, vec!["lizzie", "brickley"]);
            }
            Err(s) => panic!("unexpected error: {}", s),
        }
    }

    #[test]
    fn test_map_bad_value() {
        #[allow(dead_code)]
        #[derive(Deserialize)]
        struct A {
            bbb: String,
        }

        let mut map = BTreeMap::new();
        map.insert(
            "bbb".to_string(),
            VariableValue::Components(vec![
                "lizzie".to_string(),
                "brickley".to_string(),
            ]),
        );

        match from_map::<A, VariableValue>(&map) {
            Ok(_) => panic!("unexpected success"),
            Err(s) => {
                assert_eq!(s, "cannot deserialize sequence as a single value")
            }
        }
    }

    #[test]
    fn test_map_bad_seq() {
        #[allow(dead_code)]
        #[derive(Deserialize)]
        struct A {
            bbb: Vec<String>,
        }

        let mut map = BTreeMap::new();
        map.insert(
            "bbb".to_string(),
            VariableValue::String("doggos".to_string()),
        );

        match from_map::<A, VariableValue>(&map) {
            Ok(_) => panic!("unexpected success"),
            Err(s) => {
                assert_eq!(s, "cannot deserialize a single value as a sequence")
            }
        }
    }
}
