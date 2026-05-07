use anyhow::{Result, bail};
use ipc::{FanIndex, FanMode};
use super::EcDevice;

pub fn apply_fan_mode(ec: &EcDevice, fan: &FanIndex, mode: &FanMode) -> Result<()> {
    let thermal_policy_override: u16 = match fan {
        FanIndex::Cpu => ec.offsets.ram_thermal_policy_cpu,
        FanIndex::Gpu => ec.offsets.ram_thermal_policy_gpu,
    };

    let (policy, duty) = match mode {
        FanMode::Auto => (0x00, 0),
        FanMode::Full => (0x40, 150),
        FanMode::Custom(d) => {
            if *d > 220 {
                bail!("Requested fan duty cycle ({}) exceeds safe limit (220).", d);
            }
            (0x40, *d)
        }
    };

    ec.with_batch(|b| {
        b.write_ram(thermal_policy_override, policy)?;
        b.write_ram(*fan as u16, duty)
    })
}
