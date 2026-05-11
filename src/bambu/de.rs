use std::{fmt, marker::PhantomData};

use serde::{
    de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor},
    Deserializer,
};

pub(super) fn optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    optional::<String, D>(deserializer)
}

pub(super) fn optional_f64<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    optional::<f64, D>(deserializer)
}

pub(super) fn optional_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    optional::<i64, D>(deserializer)
}

fn optional<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: LossyValue,
    D: Deserializer<'de>,
{
    deserializer.deserialize_any(OptionalVisitor::<T>::default())
}

trait LossyValue: Sized {
    const EXPECTING: &'static str;

    fn from_i64(value: i64) -> Option<Self>;
    fn from_u64(value: u64) -> Option<Self>;
    fn from_f64(value: f64) -> Option<Self>;
    fn from_str(value: &str) -> Option<Self>;
}

impl LossyValue for String {
    const EXPECTING: &'static str = "a string or number";

    fn from_i64(value: i64) -> Option<Self> {
        Some(value.to_string())
    }

    fn from_u64(value: u64) -> Option<Self> {
        Some(value.to_string())
    }

    fn from_f64(value: f64) -> Option<Self> {
        Some(value.to_string())
    }

    fn from_str(value: &str) -> Option<Self> {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_owned())
    }
}

impl LossyValue for f64 {
    const EXPECTING: &'static str = "a number or numeric string";

    fn from_i64(value: i64) -> Option<Self> {
        Some(value as f64)
    }

    fn from_u64(value: u64) -> Option<Self> {
        Some(value as f64)
    }

    fn from_f64(value: f64) -> Option<Self> {
        Some(value)
    }

    fn from_str(value: &str) -> Option<Self> {
        value.trim().trim_end_matches('%').parse().ok()
    }
}

impl LossyValue for i64 {
    const EXPECTING: &'static str = "an integer or numeric string";

    fn from_i64(value: i64) -> Option<Self> {
        Some(value)
    }

    fn from_u64(value: u64) -> Option<Self> {
        i64::try_from(value).ok()
    }

    fn from_f64(value: f64) -> Option<Self> {
        Some(value as i64)
    }

    fn from_str(value: &str) -> Option<Self> {
        value.trim().parse::<f64>().ok().map(|value| value as i64)
    }
}

struct OptionalVisitor<T>(PhantomData<T>);

impl<T> Default for OptionalVisitor<T> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<'de, T> Visitor<'de> for OptionalVisitor<T>
where
    T: LossyValue,
{
    type Value = Option<T>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(T::EXPECTING)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(None)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(None)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(T::from_i64(value))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(T::from_u64(value))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(T::from_f64(value))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(T::from_str(value))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(&value)
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(None)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(None)
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(None)
    }
}
