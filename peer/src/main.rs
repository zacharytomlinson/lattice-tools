use lattice_peer::{
    Client, Integer, InvokeStatus, Packet, StateReadResult, Value, surface::SurfaceSummary,
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        return Err(
            "usage: lattice-peer DEVICE status|links|surface|state|invoke|watch ...".into(),
        );
    }
    let parse = |s: &String| u64::from_str_radix(s.trim_start_matches("0x"), 16);
    match args[1].as_str() {
        "watch" if (2..=3).contains(&args.len()) => {},
        "status" if args.len() == 2 => {}, "links" if (2..=4).contains(&args.len()) => {},
        "surface" if (3..=4).contains(&args.len()) => {}, "state" if args.len() == 4 => {},
        "invoke" if args.len() >= 4 => {},
        _ => return Err("usage: DEVICE watch [COUNT] | status | links [OWNER [VIA]] | surface OWNER [FILE] | state OWNER ATTRIBUTE | invoke OWNER TRANSFORMER [bool:true|u8:42|i32:-1 ...]".into()),
    }
    let mut client = Client::open(&args[0])?;
    if args[1] == "watch" {
        let count = args.get(2).map(|s| s.parse::<usize>()).transpose()?;
        let mut received = 0;
        while count.is_none_or(|limit| received < limit) {
            println!("{:?}", client.wait_event(None)?);
            received += 1;
        }
        return Ok(());
    }
    let status = client.status()?;
    let Packet::Ready {
        address,
        founder,
        channel,
        ..
    } = status
    else {
        return Err(format!("peer not ready: {status:?}").into());
    };
    println!("READY address={address:016x} founder={founder:016x} channel={channel}");
    if args[1] == "status" {
        return Ok(());
    }
    let owner = args.get(2).map(parse).transpose()?.unwrap_or(address);
    match args[1].as_str() {
        "surface" => {
            let bytes = client.read_surface(owner)?;
            let surface = SurfaceSummary::parse(&bytes)?;
            println!(
                "SURFACE owner={owner:016x} bytes={} pointer_bits={} axioms={} invariants={}",
                bytes.len(),
                surface.pointer_bits,
                surface.axioms,
                surface.invariants
            );
            for state in surface.states {
                println!(
                    "STATE reference={:016x} type={:?}",
                    state.reference, state.ty
                );
            }
            for operation in surface.transformers {
                println!(
                    "TRANSFORMER reference={:016x} arguments={:?} inputs={:?} outputs={:?}",
                    operation.reference, operation.arguments, operation.inputs, operation.outputs
                );
            }
            if let Some(path) = args.get(3) {
                std::fs::write(path, bytes)?;
            }
            return Ok(());
        }
        "state" => {
            let attribute = parse(&args[3])?;
            let response = client.read_state(owner, attribute)?;
            let Packet::State { result, .. } = response else {
                return Err(format!("read failed: {response:?}").into());
            };
            println!("STATE owner={owner:016x} attribute={attribute:016x} result={result:?}");
            return match result {
                StateReadResult::Value { .. } => Ok(()),
                _ => Err("state read unavailable".into()),
            };
        }
        "invoke" => {
            let transformer = parse(&args[3])?;
            let values: Result<Vec<_>, _> = args[4..].iter().map(|s| parse_value(s)).collect();
            let response = client.invoke(owner, transformer, &values?)?;
            let Packet::Invoked { status, .. } = response else {
                return Err(
                    format!("invocation outcome unconfirmed: {response:?}; not retried").into(),
                );
            };
            println!(
                "INVOKED owner={owner:016x} transformer={transformer:016x} status={status:?} (native result; registered changes arrive separately)"
            );
            return if status == InvokeStatus::Invoked {
                Ok(())
            } else {
                Err("native invocation failed".into())
            };
        }
        _ => {}
    }
    let via = args.get(3).map(parse).transpose()?.unwrap_or(0);
    let mut after = 0;
    let mut revision = None;
    for _ in 0..128 {
        let response = client.read_links(owner, after, via)?;
        let Packet::Link {
            revision: current,
            peer,
            round_trip_ms,
            ..
        } = response
        else {
            return Err(format!("read failed: {response:?}").into());
        };
        if revision.is_some_and(|r| r != current) {
            return Err("links changed during enumeration; restart with a new read".into());
        }
        revision = Some(current);
        if peer == 0 {
            println!("END owner={owner:016x} revision={current}");
            return Ok(());
        }
        if peer <= after {
            return Err("link cursor did not advance".into());
        }
        println!(
            "LINK owner={owner:016x} peer={peer:016x} round_trip_ms={round_trip_ms} revision={current}"
        );
        after = peer;
    }
    Err("enumeration exceeded 128 entries".into())
}

fn parse_value(text: &str) -> Result<Value, Box<dyn std::error::Error>> {
    let (kind, value) = text.split_once(':').ok_or("expected type:value")?;
    if kind == "bool" {
        return Ok(Value::boolean(value.parse()?));
    }
    let ty = match kind {
        "u8" => Integer::U8,
        "u16" => Integer::U16,
        "u32" => Integer::U32,
        "u64" => Integer::U64,
        "i8" => Integer::I8,
        "i16" => Integer::I16,
        "i32" => Integer::I32,
        "i64" => Integer::I64,
        _ => return Err("unknown scalar type".into()),
    };
    Value::integer(ty, value.parse()?).map_err(|_| "value outside declared type".into())
}
