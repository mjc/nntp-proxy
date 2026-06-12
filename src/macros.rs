#[doc(hidden)]
#[macro_export]
macro_rules! count_newtype {
    ($(#[$meta:meta])* $name:ident, $ty:ty) => {
        $(#[$meta])*
        #[repr(transparent)]
        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            Hash,
            PartialOrd,
            Ord,
            Default,
            serde::Serialize,
            serde::Deserialize,
        )]
        pub struct $name($ty);

        impl $name {
            pub const ZERO: Self = Self(0);

            #[inline]
            pub const fn new(value: $ty) -> Self {
                Self(value)
            }

            #[must_use]
            #[inline]
            pub const fn get(self) -> $ty {
                self.0
            }
        }

        impl From<$ty> for $name {
            #[inline]
            fn from(value: $ty) -> Self {
                Self::new(value)
            }
        }

        impl From<$name> for $ty {
            #[inline]
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl ::core::ops::AddAssign<$ty> for $name {
            #[inline]
            fn add_assign(&mut self, rhs: $ty) {
                self.0 += rhs;
            }
        }
    };
}
