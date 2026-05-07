use ipc::ChargeLimit;
use anyhow::{Ok, Result};
use super::EcDevice;

pub fn read_charge_limit(ec: &EcDevice) -> Result<(u8, u8)> {
    let min = ec.read_ram(ec.offsets.ram_bat_limit_min)? as u8;
    let max = ec.read_ram(ec.offsets.ram_bat_limit_max)? as u8;
    Ok((min, max))
}

pub fn read_battery_rsoc(ec: &EcDevice) -> Result<u8> {
    ec.read_ram(ec.offsets.ram_bat_rsoc)
}

pub fn apply_charge_limit(ec: &EcDevice, limit: &ChargeLimit) -> Result<()> {
    let (min, max) = limit.as_percent();

    ec.with_batch(|b| {
        b.write_ram(b.offsets.ram_bat_limit_min, min)?;
        b.write_ram(b.offsets.ram_bat_limit_max, max)
    })?;

    Ok(())
}
