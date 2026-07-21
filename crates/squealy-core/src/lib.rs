//! Shared domain functionality for `squealy`.

/// Returns the product name.
#[must_use]
pub const fn name() -> &'static str {
	"squealy"
}

#[cfg(test)]
mod tests {
	#[test]
	fn product_name_is_squealy() {
		assert_eq!(super::name(), "squealy");
	}
}
