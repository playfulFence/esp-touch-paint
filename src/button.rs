//! The board button that restores the demo picture.

use crate::ButtonResources;

cfg_select! {
    feature = "esp32s3" => {
        use esp_hal::gpio::{Input, InputConfig, Pull};

        /// The BOOT button.
        pub struct RestoreButton(Input<'static>);

        impl RestoreButton {
            pub fn new(r: ButtonResources<'static>) -> Self {
                Self(Input::new(
                    r.pin,
                    InputConfig::default().with_pull(Pull::Up),
                ))
            }

            pub fn is_pressed(&mut self) -> bool {
                self.0.is_low()
            }
        }
    }
    _ => {
        use esp_hal::{
            Blocking,
            analog::adc::{Adc, AdcConfig, AdcPin, Attenuation},
            peripherals::{ADC1, GPIO42},
        };

        /// The SET button on the Korvo-1's ADC button ladder.
        pub struct RestoreButton {
            adc: Adc<'static, ADC1<'static>, Blocking>,
            pin: AdcPin<GPIO42<'static>, ADC1<'static>>,
        }

        impl RestoreButton {
            pub fn new(r: ButtonResources<'static>) -> Self {
                let mut config = AdcConfig::new();
                let pin = config.enable_pin(r.pin, Attenuation::_11dB);
                Self {
                    adc: Adc::new(r.adc, config),
                    pin,
                }
            }

            pub fn is_pressed(&mut self) -> bool {
                let raw: u16 = self.adc.read_blocking(&mut self.pin);
                let millivolts = 2000u32.saturating_sub((raw as u32 * 4000) / 4393) as u16;
                (1605..1935).contains(&millivolts)
            }
        }
    }
}
