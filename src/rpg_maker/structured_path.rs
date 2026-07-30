//! RPG Maker 结构化位置的共用遍历与嵌套 JSON 边界。
//!
//! 这里拥有 `ObjectKey`、`ArrayIndex` 和 `DecodeJsonString` 的路径语义。读取与
//! 修改可以使用不同的 JSON 值表示和错误类型，但不能各自重新解释路径或解码层。

use std::convert::Infallible;

use serde_json::Value;

use super::text::RpgMakerLocationStep;

/// 结构化路径访问普通 JSON 节点时的稳定失败原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StructuredPathAccessError {
    ExpectedObject,
    MissingObjectKey,
    ExpectedArray,
    MissingArrayIndex,
    UnexpectedDecodeBoundary,
}

/// 结构化路径遍历、解码或重编码失败。
#[derive(Debug)]
pub(crate) enum StructuredPathError<D, E = Infallible> {
    Access(StructuredPathAccessError),
    ExpectedEncodedJsonString,
    Decode(D),
    Encode(E),
}

/// 路径遍历所需的最小 JSON 值能力。
pub(crate) trait StructuredPathValue: Sized {
    fn is_object(&self) -> bool;
    fn object_value(&self, key: &str) -> Option<&Self>;
    fn object_value_mut(&mut self, key: &str) -> Option<&mut Self>;
    fn is_array(&self) -> bool;
    fn array_value(&self, index: usize) -> Option<&Self>;
    fn array_value_mut(&mut self, index: usize) -> Option<&mut Self>;
    fn string_value(&self) -> Option<&str>;
    fn replace_with_string(&mut self, value: String);
}

impl StructuredPathValue for Value {
    fn is_object(&self) -> bool {
        self.is_object()
    }

    fn object_value(&self, key: &str) -> Option<&Self> {
        self.as_object().and_then(|object| object.get(key))
    }

    fn object_value_mut(&mut self, key: &str) -> Option<&mut Self> {
        self.as_object_mut().and_then(|object| object.get_mut(key))
    }

    fn is_array(&self) -> bool {
        self.is_array()
    }

    fn array_value(&self, index: usize) -> Option<&Self> {
        self.as_array().and_then(|array| array.get(index))
    }

    fn array_value_mut(&mut self, index: usize) -> Option<&mut Self> {
        self.as_array_mut().and_then(|array| array.get_mut(index))
    }

    fn string_value(&self) -> Option<&str> {
        self.as_str()
    }

    fn replace_with_string(&mut self, value: String) {
        *self = Self::String(value);
    }
}

/// 一种可以产生拥有型嵌套 JSON 值的解码适配器。
pub(crate) trait StructuredPathDecoder {
    type Value: StructuredPathValue;
    type Owned;
    type DecodeError;

    fn decode(&mut self, source: &str) -> Result<Self::Owned, Self::DecodeError>;
    fn value(owned: &Self::Owned) -> &Self::Value;
    fn value_mut(owned: &mut Self::Owned) -> &mut Self::Value;
}

/// 在读取能力之上支持把修改后的嵌套值编码回 JSON string。
pub(crate) trait StructuredPathCodec: StructuredPathDecoder {
    type EncodeError;

    fn encode(&mut self, value: &Self::Value) -> Result<String, Self::EncodeError>;
}

/// 把路径在第一个 `DecodeJsonString` 处分成解码前的普通路径和解码后的剩余路径。
///
/// RPG Maker 的同容器批处理与单路径读写必须共用这个边界解释，避免一方把解码
/// 标记包含在父路径中、另一方把它包含在子路径中。
pub(crate) fn split_at_decode_boundary(
    steps: &[RpgMakerLocationStep],
) -> Option<(&[RpgMakerLocationStep], &[RpgMakerLocationStep])> {
    steps
        .iter()
        .position(|step| matches!(step, RpgMakerLocationStep::DecodeJsonString))
        .map(|index| (&steps[..index], &steps[index + 1..]))
}

/// 沿完整结构化路径修改一个值，并由内向外重建全部嵌套 JSON string。
///
/// 所有解码层保留为独立拥有型值；访问、修改或任一重编码失败时，不会把部分层
/// 写回物理根。
pub(crate) fn edit_structured_path<C, R, F>(
    root: &mut C::Value,
    steps: &[RpgMakerLocationStep],
    codec: &mut C,
    edit: impl FnOnce(&mut C::Value) -> Result<R, F>,
) -> Result<Result<R, F>, StructuredPathError<C::DecodeError, C::EncodeError>>
where
    C: StructuredPathCodec,
{
    let mut decoded_layers = Vec::<C::Owned>::new();
    let mut parent_paths = Vec::<&[RpgMakerLocationStep]>::new();
    let mut remaining = steps;
    while let Some((plain, after_decode)) = split_at_decode_boundary(remaining) {
        let parent = decoded_layers.last().map_or(&*root, C::value);
        let encoded = value_at_plain_steps(parent, plain)
            .map_err(StructuredPathError::Access)?
            .string_value()
            .ok_or(StructuredPathError::ExpectedEncodedJsonString)?;
        let decoded = codec.decode(encoded).map_err(StructuredPathError::Decode)?;
        decoded_layers.push(decoded);
        parent_paths.push(plain);
        remaining = after_decode;
    }

    let target = if let Some(deepest) = decoded_layers.last_mut() {
        value_at_plain_steps_mut(C::value_mut(deepest), remaining)
            .map_err(StructuredPathError::Access)?
    } else {
        value_at_plain_steps_mut(root, steps).map_err(StructuredPathError::Access)?
    };
    let result = match edit(target) {
        Ok(result) => result,
        Err(error) => return Ok(Err(error)),
    };

    for layer_index in (0..decoded_layers.len()).rev() {
        let encoded = codec
            .encode(C::value(&decoded_layers[layer_index]))
            .map_err(StructuredPathError::Encode)?;
        let parent = if layer_index == 0 {
            &mut *root
        } else {
            C::value_mut(&mut decoded_layers[layer_index - 1])
        };
        value_at_plain_steps_mut(parent, parent_paths[layer_index])
            .map_err(StructuredPathError::Access)?
            .replace_with_string(encoded);
    }
    Ok(Ok(result))
}

/// 沿不含 `DecodeJsonString` 的路径访问一个值。
pub(crate) fn value_at_plain_steps<'a, V>(
    mut value: &'a V,
    steps: &[RpgMakerLocationStep],
) -> Result<&'a V, StructuredPathAccessError>
where
    V: StructuredPathValue,
{
    for step in steps {
        value = match step {
            RpgMakerLocationStep::ObjectKey(key) => {
                if !value.is_object() {
                    return Err(StructuredPathAccessError::ExpectedObject);
                }
                value
                    .object_value(key)
                    .ok_or(StructuredPathAccessError::MissingObjectKey)?
            }
            RpgMakerLocationStep::ArrayIndex(index) => {
                if !value.is_array() {
                    return Err(StructuredPathAccessError::ExpectedArray);
                }
                value
                    .array_value(*index)
                    .ok_or(StructuredPathAccessError::MissingArrayIndex)?
            }
            RpgMakerLocationStep::DecodeJsonString => {
                return Err(StructuredPathAccessError::UnexpectedDecodeBoundary);
            }
        };
    }
    Ok(value)
}

/// 沿不含 `DecodeJsonString` 的路径可变访问一个值。
pub(crate) fn value_at_plain_steps_mut<'a, V>(
    mut value: &'a mut V,
    steps: &[RpgMakerLocationStep],
) -> Result<&'a mut V, StructuredPathAccessError>
where
    V: StructuredPathValue,
{
    for step in steps {
        value = match step {
            RpgMakerLocationStep::ObjectKey(key) => {
                if !value.is_object() {
                    return Err(StructuredPathAccessError::ExpectedObject);
                }
                value
                    .object_value_mut(key)
                    .ok_or(StructuredPathAccessError::MissingObjectKey)?
            }
            RpgMakerLocationStep::ArrayIndex(index) => {
                if !value.is_array() {
                    return Err(StructuredPathAccessError::ExpectedArray);
                }
                value
                    .array_value_mut(*index)
                    .ok_or(StructuredPathAccessError::MissingArrayIndex)?
            }
            RpgMakerLocationStep::DecodeJsonString => {
                return Err(StructuredPathAccessError::UnexpectedDecodeBoundary);
            }
        };
    }
    Ok(value)
}
