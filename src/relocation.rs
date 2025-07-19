use crate::{Deserializer, NonTrivialObject, Serializer};
use std::io::Result;

// This needs to be a singleton to prevent different codegen units from using different copies of
// the function.
// See also: https://github.com/iex-rs/lithium/blob/9e7a1b551231/src/backend/seh.rs#L131
static BASE_ADDRESS: () = ();

#[derive(Debug)]
#[repr(transparent)]
pub(crate) struct RelocatablePtr<T>(pub(crate) *const T);

// Implement Clone/Copy even for T: !Clone/Copy
impl<T> Clone for RelocatablePtr<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for RelocatablePtr<T> {}

unsafe impl<T> NonTrivialObject for RelocatablePtr<T> {
    fn serialize_self_non_trivial<'a>(&'a self, s: &mut Serializer<'a>) {
        s.serialize_temporary((self.0 as usize).wrapping_sub((&raw const BASE_ADDRESS).addr()));
    }
    unsafe fn deserialize_self_non_trivial(d: &mut Deserializer) -> Result<Self> {
        Ok(Self(
            (&raw const BASE_ADDRESS)
                .addr()
                .wrapping_add(d.deserialize()?) as *const T,
        ))
    }
}
