//! The main export of this module is the `router!` macro, which can be used to
//! define compile time tree patterns for a router in which the terminal leaves
//! are connected to the given handler functions.
//!
//! Note that for debugging pattern matching issue, you can uncomment
//! all the `println!`s in this module.

use thiserror::Error;

/// Router error.
#[allow(missing_docs)]
#[derive(Error, Debug)]
pub enum Error {
    #[error("Found no matching pattern for the given path {0}")]
    WrongPath(String),
}

/// Find the index of a next forward slash after the given `start` index in the
/// path. When there are no more slashes, returns the index after the end of the
/// path.
///
/// # Panics
/// The given `start` must be < `path.len()`.
pub fn find_next_slash_index(path: &str, start: usize) -> usize {
    path[start..]
        .find('/')
        // Offset by the starting position
        .map(|i| start + i)
        // If not found, go to the end of path
        .unwrap_or(path.len())
}

/// Invoke the sub-handler or call the handler function with the matched
/// arguments generated by `try_match_segments`.
macro_rules! handle_match {
    // Nested router
    (
        $ctx:ident, $request:ident, $start:ident, $end:ident,
        (sub $router:tt), ( $( $matched_args:ident, )* ),
    ) => {
        // not used anymore - silence the warning
        let _ = $end;
        // Undo last '/' advance, the next pattern has to start with `/`.
        // This cannot underflow because path cannot be empty and must start
        // with `/`
        $start -= 1;
        // Invoke `handle` on the sub router
        return $router.internal_handle($ctx, $request, $start)
    };

    // Handler function that uses a request (`with_options`)
    (
        $ctx:ident, $request:ident, $start:ident, $end:ident,
        (with_options $handle:tt), ( $( $matched_args:ident, )* ),
    ) => {
        // check that we're at the end of the path - trailing slash is optional
        if !($end == $request.path.len() ||
            // ignore trailing slashes
            $end == $request.path.len() - 1 && &$request.path[$end..] == "/") {
                // we're not at the end, no match
                println!("Not fully matched");
                break
        }
        let result = $handle($ctx, $request, $( $matched_args ),* )?;
        // The handle must take care of encoding if needed and return `Vec<u8>`.
        // This is because for `storage_value` the bytes are returned verbatim
        // as read from storage.
        return Ok(result);
    };

    // Handler function that doesn't use the request, just the path args, if any
    (
        $ctx:ident, $request:ident, $start:ident, $end:ident,
        $handle:tt, ( $( $matched_args:ident, )* ),
    ) => {
        // check that we're at the end of the path - trailing slash is optional
        if !($end == $request.path.len() ||
            // ignore trailing slashes
            $end == $request.path.len() - 1 && &$request.path[$end..] == "/") {
                // we're not at the end, no match
                // println!("Not fully matched");
                break
        }
        // Check that the request is not sent with unsupported non-default
        $crate::ledger::queries::require_latest_height(&$ctx, $request)?;
        $crate::ledger::queries::require_no_proof($request)?;
        $crate::ledger::queries::require_no_data($request)?;

        // If you get a compile error from here with `expected function, found
        // queries::Storage`, you're probably missing the marker `(sub _)`
        let data = $handle($ctx, $( $matched_args ),* )?;
        // Encode the returned data with borsh
        let data = borsh::BorshSerialize::try_to_vec(&data).into_storage_result()?;
        return Ok($crate::ledger::queries::EncodedResponseQuery {
            data,
            info: Default::default(),
            proof_ops: None,
        });
    };
}

/// Using TT muncher pattern on the `$tail` pattern, this macro recursively
/// generates path matching logic that `break`s if some parts are unmatched.
macro_rules! try_match_segments {
    // sub-pattern handle - this should only be invoked if the current
    // $pattern is already matched
    (
        $ctx:ident, $request:ident, $start:ident, $end:ident,
        { $( $sub_pattern:tt $( -> $_sub_return_ty:path )? = $handle:tt, )* },
        $matched_args:tt,
        ()
    ) => {
        // Try to match each sub-patten
        $(
            // This loop never repeats, it's only used for a breaking
            // mechanism when a $pattern is not matched to skip to the
            // next one, if any
            loop {
                #[allow(unused_mut)]
                let mut $start = $start;
                let mut $end = $end;
                // Try to match, parse args and invoke $handle, will
                // break the `loop` not matched
                try_match_segments!($ctx, $request, $start, $end,
                    $handle, $matched_args, $sub_pattern
                );
            }
        )*
    };

    // Terminal tail call, invoked after when all the args in the current
    // pattern are matched and the $handle is not sub-pattern
    (
        $ctx:ident, $request:ident, $start:ident, $end:ident, $handle:tt,
        ( $( $matched_args:ident, )* ),
        ()
    ) => {
        handle_match!($ctx, $request, $start, $end, $handle, ( $( $matched_args, )* ), );
    };

    // Try to match an untyped argument, declares the expected $arg as &str
    (
        $ctx:ident, $request:ident, $start:ident, $end:ident, $handle:ident,
        ( $( $matched_args:ident, )* ),
        (
            [$arg:ident]
            $( / $( $tail:tt)/ * )?
        )
    ) => {
        let $arg = &$request.path[$start..$end];
        // Advanced index past the matched arg
        $start = $end;
        // advance past next '/', if any
        if $start + 1 < $request.path.len() {
            $start += 1;
        }
        $end = find_next_slash_index(&$request.path, $start);
        try_match_segments!($ctx, $request, $start, $end, $handle,
            ( $( $matched_args, )* $arg, ), ( $( $( $tail )/ * )? ) );
    };

    // Try to match and parse a typed argument like the case below, but with
    // the argument optional.
    // Declares the expected $arg into type $t, if it can be parsed.
    (
        $ctx:ident, $request:ident, $start:ident, $end:ident, $handle:tt,
        ( $( $matched_args:ident, )* ),
        (
            [$arg:ident : opt $arg_ty:ty]
            $( / $( $tail:tt)/ * )?
        )
    ) => {
        let $arg: Option<$arg_ty> = match $request.path[$start..$end].parse::<$arg_ty>() {
            Ok(parsed) => {
                // Only advance if optional argument is present, otherwise stay
                // in the same position for the next match, if any.

                $start = $end;
                // advance past next '/', if any
                if $start + 1 < $request.path.len() {
                    $start += 1;
                }
                $end = find_next_slash_index(&$request.path, $start);

                Some(parsed)
            },
            Err(_) =>
            {
                // If arg cannot be parsed, ignore it because it's optional
                None
            }
        };
        try_match_segments!($ctx, $request, $start, $end, $handle,
            ( $( $matched_args, )* $arg, ), ( $( $( $tail )/ * )? ) );
    };

    // Special case of the typed argument pattern below. When there are no more
    // args in the tail and the handle isn't a sub-router (its handler is
    // ident), we try to match the rest of the path till the end.
    //
    // This is specifically needed for storage methods, which have
    // `storage::Key` param that includes path-like slashes.
    //
    // Try to match and parse a typed argument, declares the expected $arg into
    // type $t, if it can be parsed
    (
        $ctx:ident, $request:ident, $start:ident, $end:ident,
        $handle:ident,
        ( $( $matched_args:ident, )* ),
        (
            [$arg:ident : $arg_ty:ty]
        )
    ) => {
        let $arg: $arg_ty;
        $end = $request.path.len();
        match $request.path[$start..$end].parse::<$arg_ty>() {
            Ok(parsed) => {
                // println!("Parsed {}", parsed);
                $arg = parsed
            },
            Err(_) =>
            {
                // println!("Cannot parse {} from {}", stringify!($arg_ty), &$request.path[$start..$end]);
                // If arg cannot be parsed, try to skip to next pattern
                break
            }
        }
        // Invoke the terminal pattern
        try_match_segments!($ctx, $request, $start, $end, $handle,
            ( $( $matched_args, )* $arg, ), () );
    };

    // One more special case of the typed argument pattern below for a handler
    // `with_options`, where we try to match the rest of the path till the end.
    //
    // This is specifically needed for storage methods, which have
    // `storage::Key` param that includes path-like slashes.
    //
    // Try to match and parse a typed argument, declares the expected $arg into
    // type $t, if it can be parsed
    (
        $ctx:ident, $request:ident, $start:ident, $end:ident,
        (with_options $handle:ident),
        ( $( $matched_args:ident, )* ),
        (
            [$arg:ident : $arg_ty:ty]
        )
    ) => {
        let $arg: $arg_ty;
        $end = $request.path.len();
        match $request.path[$start..$end].parse::<$arg_ty>() {
            Ok(parsed) => {
                println!("Parsed {}", parsed);
                $arg = parsed
            },
            Err(_) =>
            {
                println!("Cannot parse {} from {}", stringify!($arg_ty), &$request.path[$start..$end]);
                // If arg cannot be parsed, try to skip to next pattern
                break
            }
        }
        // Invoke the terminal pattern
        try_match_segments!($ctx, $request, $start, $end, (with_options $handle),
            ( $( $matched_args, )* $arg, ), () );
    };

    // Try to match and parse a typed argument, declares the expected $arg into
    // type $t, if it can be parsed
    (
        $ctx:ident, $request:ident, $start:ident, $end:ident, $handle:tt,
        ( $( $matched_args:ident, )* ),
        (
            [$arg:ident : $arg_ty:ty]
            $( / $( $tail:tt)/ * )?
        )
    ) => {
        let $arg: $arg_ty;
        match $request.path[$start..$end].parse::<$arg_ty>() {
            Ok(parsed) => {
                $arg = parsed
            },
            Err(_) =>
            {
                // println!("Cannot parse {} from {}", stringify!($arg_ty), &$request.path[$start..$end]);
                // If arg cannot be parsed, try to skip to next pattern
                break
            }
        }
        $start = $end;
        // advance past next '/', if any
        if $start + 1 < $request.path.len() {
            $start += 1;
        }
        $end = find_next_slash_index(&$request.path, $start);
        try_match_segments!($ctx, $request, $start, $end, $handle,
            ( $( $matched_args, )* $arg, ), ( $( $( $tail )/ * )? ) );
    };

    // Try to match an expected string literal
    (
        $ctx:ident, $request:ident, $start:ident, $end:ident, $handle:tt,
        ( $( $matched_args:ident, )* ),
        (
            $expected:literal
            $( / $( $tail:tt)/ * )?
        )
    ) => {
        if &$request.path[$start..$end] == $expected {
            // Advanced index past the matched arg
            // println!("Matched literal {}", $expected);
            $start = $end;
        } else {
            // println!("{} doesn't match literal {}", &$request.path[$start..$end], $expected);
            // Try to skip to next pattern
            break;
        }
        // advance past next '/', if any
        if $start + 1 < $request.path.len() {
            $start += 1;
        }
        $end = find_next_slash_index(&$request.path, $start);
        try_match_segments!($ctx, $request, $start, $end, $handle,
            ( $( $matched_args, )* ), ( $( $( $tail )/ * )? ) );
    };
}

/// Generate a function that tries to match the given pattern and `break`s if
/// any of its parts are unmatched. This layer will check that the path starts
/// with `/` and then invoke `try_match_segments` TT muncher that goes through
/// the patterns.
macro_rules! try_match {
    ($ctx:ident, $request:ident, $start:ident, $handle:tt, $segments:tt) => {
        // check that the initial char is '/'
        if $request.path.is_empty() || &$request.path[..1] != "/" {
            // println!("Missing initial slash");
            break;
        }
        // advance past initial '/'
        $start += 1;
        // Path is too short to match
        if $start >= $request.path.len() {
            // println!("Path is too short");
            break;
        }
        let mut end = find_next_slash_index(&$request.path, $start);
        try_match_segments!(
            $ctx,
            $request,
            $start,
            end,
            $handle,
            (),
            $segments
        );
    };
}

/// Convert literal pattern into a `&[&'static str]`
// TODO sub router pattern is not yet used
#[allow(unused_macros)]
macro_rules! pattern_to_prefix {
    ( ( $( $pattern:literal )/ * ) ) => {
        &[$( $pattern ),*]
    };
    ( $pattern:tt ) => {
        compile_error!("sub-router cannot have non-literal prefix patterns")
    };
}

/// Turn patterns and their handlers into methods for the router, where each
/// dynamic pattern is turned into a parameter for the method.
macro_rules! pattern_and_handler_to_method {
    // Special terminal rule for `storage_value` handle from
    // `shared/src/ledger/queries/shell.rs` that returns `Vec<u8>` which should
    // not be decoded from response.data, but instead return as is
    (
        ( $( $param:tt: $param_ty:ty ),* )
        [ $( { $prefix:expr } ),* ]
        $return_type:path,
        (with_options storage_value),
        ()
    ) => {
        // paste! used to construct the `fn $handle_path`'s name.
        paste::paste! {
            #[allow(dead_code)]
            #[doc = "Get a path to query `storage_value`."]
            pub fn storage_value_path(&self, $( $param: &$param_ty ),* ) -> String {
                itertools::join(
                    [ Some(std::borrow::Cow::from(&self.prefix)), $( $prefix ),* ]
                    .into_iter()
                    .filter_map(|x| x), "/")
            }

            #[allow(dead_code)]
            #[allow(clippy::too_many_arguments)]
            #[cfg(any(test, feature = "async-client"))]
            #[doc = "Request value with optional data (used for e.g. \
                `dry_run_tx`), optionally specified height (supported for \
                `storage_value`) and optional proof (supported for \
                `storage_value` and `storage_prefix`) from `storage_value`."]
            pub async fn storage_value<CLIENT>(&self, client: &CLIENT,
                data: Option<Vec<u8>>,
                height: Option<$crate::types::storage::BlockHeight>,
                prove: bool,
                $( $param: &$param_ty ),*
            )
                -> std::result::Result<
                    $crate::ledger::queries::ResponseQuery<Vec<u8>>,
                    <CLIENT as $crate::ledger::queries::Client>::Error
                >
                where CLIENT: $crate::ledger::queries::Client + std::marker::Sync {
                    println!("IMMA VEC!!!!!!");
                    let path = self.storage_value_path( $( $param ),* );

                    let $crate::ledger::queries::ResponseQuery {
                        data, info, proof_ops
                    } = client.request(path, data, height, prove).await?;

                    Ok($crate::ledger::queries::ResponseQuery {
                        data,
                        info,
                        proof_ops,
                    })
            }
        }
    };

    // terminal rule for $handle that uses request (`with_options`)
    (
        ( $( $param:tt: $param_ty:ty ),* )
        [ $( { $prefix:expr } ),* ]
        $return_type:path,
        (with_options $handle:tt),
        ()
    ) => {
        // paste! used to construct the `fn $handle_path`'s name.
        paste::paste! {
            #[allow(dead_code)]
            #[doc = "Get a path to query `" $handle "`."]
            pub fn [<$handle _path>](&self, $( $param: &$param_ty ),* ) -> String {
                itertools::join(
                    [ Some(std::borrow::Cow::from(&self.prefix)), $( $prefix ),* ]
                    .into_iter()
                    .filter_map(|x| x), "/")
            }

            #[allow(dead_code)]
            #[allow(clippy::too_many_arguments)]
            #[cfg(any(test, feature = "async-client"))]
            #[doc = "Request value with optional data (used for e.g. \
                `dry_run_tx`), optionally specified height (supported for \
                `storage_value`) and optional proof (supported for \
                `storage_value` and `storage_prefix`) from `" $handle "`."]
            pub async fn $handle<CLIENT>(&self, client: &CLIENT,
                data: Option<Vec<u8>>,
                height: Option<$crate::types::storage::BlockHeight>,
                prove: bool,
                $( $param: &$param_ty ),*
            )
                -> std::result::Result<
                    $crate::ledger::queries::ResponseQuery<$return_type>,
                    <CLIENT as $crate::ledger::queries::Client>::Error
                >
                where CLIENT: $crate::ledger::queries::Client + std::marker::Sync {
                    println!("IMMA not a VEC!!!!!!");
                    let path = self.[<$handle _path>]( $( $param ),* );

                    let $crate::ledger::queries::ResponseQuery {
                        data, info, proof_ops
                    } = client.request(path, data, height, prove).await?;

                    let decoded: $return_type =
                        borsh::BorshDeserialize::try_from_slice(&data[..])?;

                    Ok($crate::ledger::queries::ResponseQuery {
                        data: decoded,
                        info,
                        proof_ops,
                    })
            }
        }
    };

    // terminal rule that $handle that doesn't use request
    (
        ( $( $param:tt: $param_ty:ty ),* )
        [ $( { $prefix:expr } ),* ]
        $return_type:path,
        $handle:tt,
        ()
    ) => {
        // paste! used to construct the `fn $handle_path`'s name.
        paste::paste! {
            #[allow(dead_code)]
            #[doc = "Get a path to query `" $handle "`."]
            pub fn [<$handle _path>](&self, $( $param: &$param_ty ),* ) -> String {
                itertools::join(
                    [ Some(std::borrow::Cow::from(&self.prefix)), $( $prefix ),* ]
                    .into_iter()
                    .filter_map(|x| x), "/")
            }

            #[allow(dead_code)]
            #[allow(clippy::too_many_arguments)]
            #[cfg(any(test, feature = "async-client"))]
            #[doc = "Request a simple borsh-encoded value from `" $handle "`, \
                without any additional request data, specified block height or \
                proof."]
            pub async fn $handle<CLIENT>(&self, client: &CLIENT,
                $( $param: &$param_ty ),*
            )
                -> std::result::Result<
                    $return_type,
                    <CLIENT as $crate::ledger::queries::Client>::Error
                >
                where CLIENT: $crate::ledger::queries::Client + std::marker::Sync {
                    let path = self.[<$handle _path>]( $( $param ),* );

                    let data = client.simple_request(path).await?;

                    let decoded: $return_type =
                        borsh::BorshDeserialize::try_from_slice(&data[..])?;
                    Ok(decoded)
            }
        }
    };

    // sub-pattern
    (
        $param:tt
        $prefix:tt
        $( $_return_type:path )?,
        { $( $sub_pattern:tt $( -> $sub_return_ty:path )? = $handle:tt, )* },
        $pattern:tt
    ) => {
        $(
            // join pattern with each sub-pattern
            pattern_and_handler_to_method!(
                $param
                $prefix
                $( $sub_return_ty )?, $handle, $pattern, $sub_pattern
            );
        )*
    };

    // literal string arg
    (
        ( $( $param:tt: $param_ty:ty ),* )
        [ $( { $prefix:expr } ),* ]
        $( $return_type:path )?,
        $handle:tt,
        ( $pattern:literal $( / $tail:tt )* )
    ) => {
        pattern_and_handler_to_method!(
            ( $( $param: $param_ty ),* )
            [ $( { $prefix }, )* { std::option::Option::Some(std::borrow::Cow::from($pattern)) } ]
            $( $return_type )?, $handle, ( $( $tail )/ * )
        );
    };

    // untyped arg
    (
        ( $( $param:tt: $param_ty:ty ),* )
        [ $( { $prefix:expr } ),* ]
        $( $return_type:path )?,
        $handle:tt,
        ( [$name:tt] $( / $tail:tt )* )
    ) => {
        pattern_and_handler_to_method!(
            ( $( $param: $param_ty, )* $name: str )
            [ $( { $prefix }, )* { std::option::Option::Some(std::borrow::Cow::from($name)) } ]
            $( $return_type )?, $handle, ( $( $tail )/ * )
        );
    };

    // typed arg
    (
        ( $( $param:tt: $param_ty:ty ),* )
        [ $( { $prefix:expr } ),* ]
        $( $return_type:path )?,
        $handle:tt,
        ( [$name:tt: $type:ty] $( / $tail:tt )* )
    ) => {
        pattern_and_handler_to_method!(
            ( $( $param: $param_ty, )* $name: $type )
            [ $( { $prefix }, )* { std::option::Option::Some(std::borrow::Cow::from($name.to_string())) } ]
            $( $return_type )?, $handle, ( $( $tail )/ * )
        );
    };

    // opt typed arg
    (
        ( $( $param:tt: $param_ty:ty ),* )
        [ $( { $prefix:expr } ),* ]
        $( $return_type:path )?,
        $handle:tt,
        ( [$name:tt: opt $type:ty] $( / $tail:tt )* )
    ) => {
        pattern_and_handler_to_method!(
            ( $( $param: $param_ty, )* $name: std::option::Option<$type> )
            [ $( { $prefix }, )* { $name.map(|arg| std::borrow::Cow::from(arg.to_string())) } ]
            $( $return_type )?, $handle, ( $( $tail )/ * )
        );
    };

    // join pattern with sub-pattern
    (
        ( $( $param:tt: $param_ty:ty ),* )
        [ $( { $prefix:expr } ),* ]
        $( $return_type:path )?,
        $handle:tt,
        ( $( $pattern:tt )/ * ), ( $( $sub_pattern:tt )/ * )
    ) => {
        pattern_and_handler_to_method!(
            ( $( $param: $param_ty ),* )
            [ $( { $prefix }, )* ]
            $( $return_type )?,
            $handle, ( $( $pattern / )* $( $sub_pattern )/ * )
        );
    };
}

/// TT muncher macro that generates a `struct $name` with methods for all its
/// handlers.
macro_rules! router_type {
    // terminal rule
    ($name:ident { $( $methods:item )* }, ) => {
        paste::paste! {
            #[doc = "`" $name "`path router type"]
            pub struct $name {
                prefix: String,
            }

            impl $name {
                #[doc = "Construct this router as a root router"]
                pub const fn new() -> Self {
                    Self {
                        prefix: String::new(),
                    }
                }

                #[allow(dead_code)]
                #[doc = "Construct this router as a sub-router at the given prefix path"]
                pub const fn sub(prefix: String) -> Self {
                    Self {
                        prefix,
                    }
                }

                // paste the generated methods
                $( $methods )*
            }
        }
    };

    // a sub router - recursion
    (
        $name:ident { $( $methods:item )* },
        $pattern:tt = (sub $router:ident)
        $( ,$tail_pattern:tt $( -> $tail_return_type:path )? = $tail:tt )*
    ) => {
        paste::paste! {
            router_type!{
                $name {
                    #[doc = "`" $name "` sub-router"]
                    pub fn [<$router:camel:snake>](&self) -> [<$router:camel>] {
                        // prefix for a sub can only contain literals
                        let current_prefix: &[&'static str] = pattern_to_prefix!($pattern);
                        let path = [&[self.prefix.as_str()][..], current_prefix].concat().join("/");
                        [<$router:camel>]::sub(path)
                    }
                    $( $methods )*
                },
                $( $tail_pattern $( -> $tail_return_type )? = $tail ),*
            }
        }
    };

    // a sub-pattern - add a method for each handle inside it
    (
        $name:ident
        { $( $methods:item )* },
        $pattern:tt = { $( $sub_pattern:tt $( -> $sub_return_ty:path )? = $handle:tt, )* }
        $( ,$tail_pattern:tt $( -> $tail_return_type:path )? = $tail:tt )*
    ) => {
        router_type!{
            $name {
                $(
                    // join pattern with each sub-pattern
                    pattern_and_handler_to_method!( () [] $( $sub_return_ty )?, $handle,
                        $pattern, $sub_pattern
                    );
                )*
                $( $methods )*
            },
            $( $tail_pattern $( -> $tail_return_type )? = $tail ),*
        }
    };

    // pattern with a handle - add a method for the handle
    (
        $name:ident
        { $( $methods:item )* },
        $pattern:tt -> $return_type:path = $handle:tt
        $( ,$tail_pattern:tt $( -> $tail_return_type:path )? = $tail:tt )*
    ) => {
        router_type!{
            $name {
                pattern_and_handler_to_method!( () [] $return_type, $handle, $pattern );
                $( $methods )*
            },
            $( $tail_pattern $( -> $tail_return_type )? = $tail ),*
        }
    };
}

/// Compile time tree patterns router with type-safe dynamic parameter parsing,
/// automatic routing, type-safe path constructors and optional client query
/// methods (enabled with `feature = "async-client"`).
///
/// The `router!` macro implements greedy matching algorithm.
///
/// ## Examples
///
/// ```rust,ignore
/// router! {ROOT,
///   // This pattern matches `/pattern_a/something`, where `something` can be
///   // parsed with `FromStr` into `ArgType`.
///   ( "pattern_a" / [typed_dynamic_arg: ArgType] ) -> ReturnType = handler,
///
///   ( "pattern_b" / [optional_dynamic_arg: opt ArgType] ) -> ReturnType =
/// handler,
///
///   // Untyped dynamic arg is a string slice `&str`
///   ( "pattern_c" / [untyped_dynamic_arg] ) -> ReturnType = handler,
///
///   // The handler additionally receives the `RequestQuery`, which can have
///   // some data attached, specified block height and ask for a proof. It
///   // returns `EncodedResponseQuery` (the `data` must be encoded, if
///   // necessary), which can have some `info` string and a proof.
///   ( "pattern_d" ) -> ReturnType = (with_options handler),
///
///   ( "another" / "pattern" / "that" / "goes" / "deep" ) -> ReturnType = handler,
///
///   // Inlined sub-tree
///   ( "subtree" / [this_is_fine: ArgType] ) = {
///     ( "a" ) -> u64 = a_handler,
///     ( "b" / [another_arg] ) -> u64 = b_handler,
///   }
///
///   // Imported sub-router - The prefix can only have literal segments
///   ( "sub" / "no_dynamic_args" ) = (sub SUB_ROUTER),
/// }
///
/// router! {SUB_ROUTER,
///   ( "pattern" ) -> ReturnType = handler,
/// }
/// ```
///
/// Handler functions used in the patterns should have the expected signature:
/// ```rust,ignore
/// fn handler<D, H>(ctx: RequestCtx<'_, D, H>, args ...)
///   -> storage_api::Result<ReturnType>
/// where
///     D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
///     H: 'static + StorageHasher + Sync;
/// ```
///
/// If the handler wants to support request options, it can be defined as
/// `(with_options $handler)` and then the expected signature is:
/// ```rust,ignore
/// fn handler<D, H>(ctx: RequestCtx<'_, D, H>, request: &RequestQuery, args
/// ...)   -> storage_api::Result<ResponseQuery<ReturnType>>
/// where
///     D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
///     H: 'static + StorageHasher + Sync;
/// ```
#[macro_export]
macro_rules! router {
    { $name:ident, $( $pattern:tt $( -> $return_type:path )? = $handle:tt , )* } => (

	// `paste!` is used to convert the $name cases for a derived type and function name
	paste::paste! {

        router_type!{[<$name:camel>] {}, $( $pattern $( -> $return_type )? = $handle ),* }

		impl $crate::ledger::queries::Router for [<$name:camel>] {
            // TODO: for some patterns, there's unused assignment of `$end`
            #[allow(unused_assignments)]
            fn internal_handle<D, H>(
			    &self,
                ctx: $crate::ledger::queries::RequestCtx<'_, D, H>,
                request: &$crate::ledger::queries::RequestQuery,
                start: usize
            ) -> $crate::ledger::storage_api::Result<$crate::ledger::queries::EncodedResponseQuery>
            where
                D: 'static + $crate::ledger::storage::DB + for<'iter> $crate::ledger::storage::DBIter<'iter> + Sync,
                H: 'static + $crate::ledger::storage::StorageHasher + Sync,
            {

                // Import for `.into_storage_result()`
                use $crate::ledger::storage_api::ResultExt;

                // Import helper from this crate used inside the macros
                use $crate::ledger::queries::router::find_next_slash_index;

				$(
                    // This loop never repeats, it's only used for a breaking
                    // mechanism when a $pattern is not matched to skip to the
                    // next one, if any
                    loop {
                        let mut start = start;
                        // Try to match, parse args and invoke $handle, will
                        // break the `loop` not matched
                        try_match!(ctx, request, start, $handle, $pattern);
                    }
                )*

				return Err(
                    $crate::ledger::queries::router::Error::WrongPath(request.path.clone()))
                    .into_storage_result();
			}
		}

		#[doc = "`" $name "` path router"]
		pub const $name: [<$name:camel>] = [<$name:camel>]::new();
	}

    );
}

/// You can expand the `handlers!` macro invocation with e.g.:
/// ```shell
/// cargo expand ledger::queries::router::test_rpc_handlers --features "ferveo-tpke, ibc-mocks, testing, wasm-runtime, tendermint-rpc" --tests --lib
/// ```
#[cfg(test)]
mod test_rpc_handlers {
    use borsh::BorshSerialize;

    use crate::ledger::queries::{
        EncodedResponseQuery, RequestCtx, RequestQuery, ResponseQuery,
    };
    use crate::ledger::storage::{DBIter, StorageHasher, DB};
    use crate::ledger::storage_api::{self, ResultExt};
    use crate::types::storage::Epoch;
    use crate::types::token;

    /// A little macro to generate boilerplate for RPC handler functions.
    /// These are implemented to return their name as a String, joined by
    /// slashes with their argument values turned `to_string()`, if any.
    macro_rules! handlers {
        (
            // name and params, if any
            $( $name:ident $( ( $( $param:ident: $param_ty:ty ),* ) )? ),*
            // optional trailing comma
            $(,)? ) => {
            $(
                pub fn $name<D, H>(
                    _ctx: RequestCtx<'_, D, H>,
                    $( $( $param: $param_ty ),* )?
                ) -> storage_api::Result<String>
                where
                    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
                    H: 'static + StorageHasher + Sync,
                {
                    let data = stringify!($name).to_owned();
                    $( $(
                        let data = format!("{data}/{}", $param);
                    )* )?
                    Ok(data)
                }
            )*
        };
    }

    // Generate handler functions for the router below
    handlers!(
        a,
        b0i,
        b0ii,
        b1,
        b2i(balance: token::Amount),
        b3(a1: token::Amount, a2: token::Amount, a3: token::Amount),
        b3i(a1: token::Amount, a2: token::Amount, a3: token::Amount),
        b3ii(a1: token::Amount, a2: token::Amount, a3: token::Amount),
        x,
        y(untyped_arg: &str),
        z(untyped_arg: &str),
    );

    /// This handler is hand-written, because the test helper macro doesn't
    /// support optional args.
    pub fn b3iii<D, H>(
        _ctx: RequestCtx<'_, D, H>,
        a1: token::Amount,
        a2: token::Amount,
        a3: Option<token::Amount>,
    ) -> storage_api::Result<String>
    where
        D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
        H: 'static + StorageHasher + Sync,
    {
        let data = "b3iii".to_owned();
        let data = format!("{data}/{}", a1);
        let data = format!("{data}/{}", a2);
        let data = a3.map(|a3| format!("{data}/{}", a3)).unwrap_or(data);
        Ok(data)
    }

    /// This handler is hand-written, because the test helper macro doesn't
    /// support optional args.
    pub fn b3iiii<D, H>(
        _ctx: RequestCtx<'_, D, H>,
        a1: token::Amount,
        a2: token::Amount,
        a3: Option<token::Amount>,
        a4: Option<Epoch>,
    ) -> storage_api::Result<String>
    where
        D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
        H: 'static + StorageHasher + Sync,
    {
        let data = "b3iiii".to_owned();
        let data = format!("{data}/{}", a1);
        let data = format!("{data}/{}", a2);
        let data = a3.map(|a3| format!("{data}/{}", a3)).unwrap_or(data);
        let data = a4.map(|a4| format!("{data}/{}", a4)).unwrap_or(data);
        Ok(data)
    }

    /// This handler is hand-written, because the test helper macro doesn't
    /// support handlers with `with_options`.
    pub fn c<D, H>(
        _ctx: RequestCtx<'_, D, H>,
        _request: &RequestQuery,
    ) -> storage_api::Result<EncodedResponseQuery>
    where
        D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
        H: 'static + StorageHasher + Sync,
    {
        let data = "c".to_owned().try_to_vec().into_storage_result()?;
        Ok(ResponseQuery {
            data,
            ..ResponseQuery::default()
        })
    }
}

/// You can expand the `router!` macro invocation with e.g.:
/// ```shell
/// cargo expand ledger::queries::router::test_rpc --features "ferveo-tpke, ibc-mocks, testing, wasm-runtime, tendermint-rpc" --tests --lib
/// ```
#[cfg(test)]
mod test_rpc {
    use super::test_rpc_handlers::*;
    use crate::types::storage::Epoch;
    use crate::types::token;

    // Setup an RPC router for testing
    router! {TEST_RPC,
        ( "sub" ) = (sub TEST_SUB_RPC),
        ( "a" ) -> String = a,
        ( "b" ) = {
            ( "0" ) = {
                ( "i" ) -> String = b0i,
                ( "ii" ) -> String = b0ii,
            },
            ( "1" ) -> String = b1,
            ( "2" ) = {
                ( "i" / [balance: token::Amount] ) -> String = b2i,
            },
            ( "3" / [a1: token::Amount] / [a2: token::Amount] ) = {
                ( "i" / [a3: token:: Amount] ) -> String = b3i,
                ( [a3: token:: Amount] ) -> String = b3,
                ( [a3: token:: Amount] / "ii" ) -> String = b3ii,
                ( [a3: opt token::Amount] / "iii" ) -> String = b3iii,
                ( "iiii" / [a3: opt token::Amount] / "xyz" / [a4: opt Epoch] ) -> String = b3iiii,
            },
        },
        ( "c" ) -> String = (with_options c),
    }

    router! {TEST_SUB_RPC,
        ( "x" ) -> String = x,
        ( "y" / [untyped_arg] ) -> String = y,
        ( "z" / [untyped_arg] ) -> String = z,
    }
}

#[cfg(test)]
mod test {
    use super::test_rpc::TEST_RPC;
    use crate::ledger::queries::testing::TestClient;
    use crate::ledger::queries::{RequestCtx, RequestQuery, Router};
    use crate::ledger::storage_api;
    use crate::types::storage::Epoch;
    use crate::types::token;

    /// Test all the possible paths in `TEST_RPC` router.
    #[tokio::test]
    async fn test_router_macro() -> storage_api::Result<()> {
        let client = TestClient::new(TEST_RPC);

        // Test request with an invalid path
        let request = RequestQuery {
            path: "/invalid".to_owned(),
            ..RequestQuery::default()
        };
        let ctx = RequestCtx {
            storage: &client.storage,
            vp_wasm_cache: client.vp_wasm_cache.clone(),
            tx_wasm_cache: client.tx_wasm_cache.clone(),
        };
        let result = TEST_RPC.handle(ctx, &request);
        assert!(result.is_err());

        // Test requests to valid paths using the router's methods

        let result = TEST_RPC.a(&client).await.unwrap();
        assert_eq!(result, "a");

        let result = TEST_RPC.b0i(&client).await.unwrap();
        assert_eq!(result, "b0i");

        let result = TEST_RPC.b0ii(&client).await.unwrap();
        assert_eq!(result, "b0ii");

        let result = TEST_RPC.b1(&client).await.unwrap();
        assert_eq!(result, "b1");

        let balance = token::Amount::from(123_000_000);
        let result = TEST_RPC.b2i(&client, &balance).await.unwrap();
        assert_eq!(result, format!("b2i/{balance}"));

        let a1 = token::Amount::from(345);
        let a2 = token::Amount::from(123_000);
        let a3 = token::Amount::from(1_000_999);
        let result = TEST_RPC.b3(&client, &a1, &a2, &a3).await.unwrap();
        assert_eq!(result, format!("b3/{a1}/{a2}/{a3}"));

        let result = TEST_RPC.b3i(&client, &a1, &a2, &a3).await.unwrap();
        assert_eq!(result, format!("b3i/{a1}/{a2}/{a3}"));

        let result = TEST_RPC.b3ii(&client, &a1, &a2, &a3).await.unwrap();
        assert_eq!(result, format!("b3ii/{a1}/{a2}/{a3}"));

        let result =
            TEST_RPC.b3iii(&client, &a1, &a2, &Some(a3)).await.unwrap();
        assert_eq!(result, format!("b3iii/{a1}/{a2}/{a3}"));

        let result = TEST_RPC.b3iii(&client, &a1, &a2, &None).await.unwrap();
        assert_eq!(result, format!("b3iii/{a1}/{a2}"));

        let result = TEST_RPC
            .b3iiii(&client, &a1, &a2, &Some(a3), &None)
            .await
            .unwrap();
        assert_eq!(result, format!("b3iiii/{a1}/{a2}/{a3}"));

        let a4 = Epoch::from(10);
        let result = TEST_RPC
            .b3iiii(&client, &a1, &a2, &Some(a3), &Some(a4))
            .await
            .unwrap();
        assert_eq!(result, format!("b3iiii/{a1}/{a2}/{a3}/{a4}"));

        let result = TEST_RPC
            .b3iiii(&client, &a1, &a2, &None, &None)
            .await
            .unwrap();
        assert_eq!(result, format!("b3iiii/{a1}/{a2}"));

        let result = TEST_RPC.c(&client, None, None, false).await.unwrap();
        assert_eq!(result.data, format!("c"));

        let result = TEST_RPC.test_sub_rpc().x(&client).await.unwrap();
        assert_eq!(result, format!("x"));

        let arg = "test123";
        let result = TEST_RPC.test_sub_rpc().y(&client, arg).await.unwrap();
        assert_eq!(result, format!("y/{arg}"));

        let arg = "test321";
        let result = TEST_RPC.test_sub_rpc().z(&client, arg).await.unwrap();
        assert_eq!(result, format!("z/{arg}"));

        Ok(())
    }
}
