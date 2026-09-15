//! Bounded protobuf wire reader for the small, read-only Cursor RPC surface.
//! Unknown proto3 fields are skipped by consumers; no generated/vendor code.

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum WireValue<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Field<'a> {
    pub number: u32,
    pub value: WireValue<'a>,
}

fn varint(input: &mut &[u8]) -> Result<u64, &'static str> {
    let mut value = 0;
    for index in 0..10 {
        let (&byte, rest) = input.split_first().ok_or("invalid_response")?;
        *input = rest;
        if index == 9 && byte > 1 {
            return Err("invalid_response");
        }
        value |= u64::from(byte & 127) << (index * 7);
        if byte & 128 == 0 {
            return Ok(value);
        }
    }
    Err("invalid_response")
}

fn take<'a>(input: &mut &'a [u8], length: usize) -> Result<&'a [u8], &'static str> {
    if length > input.len() {
        return Err("invalid_response");
    }
    let (value, rest) = input.split_at(length);
    *input = rest;
    Ok(value)
}

pub(crate) fn fields(mut input: &[u8]) -> Result<Vec<Field<'_>>, &'static str> {
    if input.len() > super::MAX_RESPONSE_BYTES {
        return Err("response_too_large");
    }
    let mut result = Vec::new();
    while !input.is_empty() {
        if result.len() >= 50_000 {
            return Err("response_too_large");
        }
        let tag = varint(&mut input)?;
        let number = tag >> 3;
        if number == 0 || number > 0x1fff_ffff {
            return Err("invalid_response");
        }
        let value = match tag & 7 {
            0 => WireValue::Varint(varint(&mut input)?),
            1 => WireValue::Fixed64(u64::from_le_bytes(
                take(&mut input, 8)?
                    .try_into()
                    .map_err(|_| "invalid_response")?,
            )),
            2 => {
                let length =
                    usize::try_from(varint(&mut input)?).map_err(|_| "invalid_response")?;
                WireValue::Bytes(take(&mut input, length)?)
            }
            5 => WireValue::Fixed32(u32::from_le_bytes(
                take(&mut input, 4)?
                    .try_into()
                    .map_err(|_| "invalid_response")?,
            )),
            _ => return Err("invalid_response"),
        };
        result.push(Field {
            number: number as u32,
            value,
        });
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_malformed_and_unknown_fields() {
        for bytes in [
            &[0][..],
            &[10, 9, 1],
            &[9, 1],
            &[15],
            &[128],
            &[8, 255, 255, 255, 255, 255, 255, 255, 255, 255, 2],
        ] {
            assert_eq!(fields(bytes), Err("invalid_response"));
        }
        assert_eq!(
            fields(&[8, 150, 1, 18, 1, b'x']).unwrap(),
            vec![
                Field {
                    number: 1,
                    value: WireValue::Varint(150)
                },
                Field {
                    number: 2,
                    value: WireValue::Bytes(b"x")
                },
            ]
        );
        assert!(fields(&[]).unwrap().is_empty());
    }
}
