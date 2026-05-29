//! Minimal local `quote!` support used by squealy's proc macros.

extern crate proc_macro;

/// Converts a value into source tokens for interpolation.
pub trait ToQuote {
    fn to_quote(&self) -> String;
}

impl<T> ToQuote for T
where
    T: std::fmt::Display,
{
    fn to_quote(&self) -> String {
        self.to_string()
    }
}

/// Build a proc-macro token stream from quoted Rust tokens.
#[macro_export]
macro_rules! quote {
    ($($tokens:tt)*) => {{
        let mut tokens = ::std::string::String::new();
        $crate::__quote_tokens!(tokens; $($tokens)*);
        tokens
            .parse::<::proc_macro::TokenStream>()
            .expect("quote! output should parse")
    }};
}

#[doc(hidden)]
#[macro_export]
macro_rules! __quote_tokens {
    ($out:ident;) => {};
    ($out:ident; # $value:ident $($rest:tt)*) => {{
        use $crate::ToQuote as _;
        $out.push_str(&$value.to_quote());
        $out.push(' ');
        $crate::__quote_tokens!($out; $($rest)*);
    }};
    ($out:ident; ($($inner:tt)*) $($rest:tt)*) => {{
        $out.push('(');
        $crate::__quote_tokens!($out; $($inner)*);
        $out.push(')');
        $out.push(' ');
        $crate::__quote_tokens!($out; $($rest)*);
    }};
    ($out:ident; {$($inner:tt)*} $($rest:tt)*) => {{
        $out.push('{');
        $crate::__quote_tokens!($out; $($inner)*);
        $out.push('}');
        $out.push(' ');
        $crate::__quote_tokens!($out; $($rest)*);
    }};
    ($out:ident; [$($inner:tt)*] $($rest:tt)*) => {{
        $out.push('[');
        $crate::__quote_tokens!($out; $($inner)*);
        $out.push(']');
        $out.push(' ');
        $crate::__quote_tokens!($out; $($rest)*);
    }};
    ($out:ident; $token:tt $($rest:tt)*) => {{
        $out.push_str(::std::stringify!($token));
        $out.push(' ');
        $crate::__quote_tokens!($out; $($rest)*);
    }};
}
