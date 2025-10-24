use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager as BtleManager, Peripheral, PeripheralId};
use midir::{MidiOutput, MidiOutputConnection};
use once_cell::sync::Lazy;
use tokio::sync::Mutex;
use tokio::time;
use uuid::Uuid;

use crate::midi::sink::{MidiSink, MidiSinkInfo, MidiTransport, SharedMidiSink};

const CLIENT_NAME: &str = "midi-piano-rs";
const SCAN_TIMEOUT: Duration = Duration::from_secs(2);

static USB_NAMESPACE: Lazy<Uuid> =
    Lazy::new(|| Uuid::from_u128(0xdea27421_4dbe_474b_99ac_5a4a3f7bf110));
static BLE_NAMESPACE: Lazy<Uuid> =
    Lazy::new(|| Uuid::from_u128(0x5a08d524_f585_4a4f_b4bd_a3e4f82345fb));

const BLE_MIDI_SERVICE_UUID: Uuid = Uuid::from_u128(0x03b80e5a_ede8_4b33_a751_6ce34ec4c700);
const BLE_MIDI_CHARACTERISTIC_UUID: Uuid = Uuid::from_u128(0x7772e5db_3868_4112_a1a9_f2669d106bf3);

#[derive(Clone, Debug)]
pub struct MidiDeviceDescriptor {
    pub info: MidiSinkInfo,
    pub kind: DeviceKind,
}

#[derive(Clone, Debug)]
pub enum DeviceKind {
    Usb(UsbDevice),
    Ble(BleDevice),
}

#[derive(Clone, Debug)]
pub struct UsbDevice {
    pub port_id: String,
    pub port_name: String,
}

#[derive(Clone, Debug)]
pub struct BleDevice {
    pub adapter: Adapter,
    pub peripheral_id: PeripheralId,
    pub name: String,
}

pub struct MidiDeviceManager {
    bt_manager: Option<BtleManager>,
    devices: HashMap<Uuid, MidiDeviceDescriptor>,
}

impl MidiDeviceManager {
    pub fn new() -> Self {
        Self {
            bt_manager: None,
            devices: HashMap::new(),
        }
    }

    pub async fn refresh(&mut self) -> Result<Vec<MidiDeviceDescriptor>> {
        let mut descriptors = match self.enumerate_usb_devices() {
            Ok(list) => list,
            Err(err) => {
                log::warn!("failed to enumerate USB MIDI outputs: {err:?}");
                Vec::new()
            }
        };

        if self.bt_manager.is_none() {
            match BtleManager::new().await {
                Ok(manager) => self.bt_manager = Some(manager),
                Err(err) => {
                    log::warn!("BLE manager not available: {err}");
                }
            }
        }

        if let Some(manager) = &self.bt_manager {
            match self.enumerate_ble_devices(manager).await {
                Ok(mut ble_devices) => descriptors.append(&mut ble_devices),
                Err(err) => log::warn!("failed to scan BLE devices: {err:?}"),
            }
        }

        self.devices.clear();
        for descriptor in &descriptors {
            self.devices.insert(descriptor.info.id, descriptor.clone());
        }

        descriptors.sort_by(|a, b| a.info.name.cmp(&b.info.name));
        Ok(descriptors)
    }

    pub async fn connect(&self, id: &Uuid) -> Result<SharedMidiSink> {
        let descriptor = self
            .devices
            .get(id)
            .cloned()
            .with_context(|| format!("unknown device id {id}"))?;

        match descriptor.kind {
            DeviceKind::Usb(device) => self.connect_usb(&descriptor.info, device).await,
            DeviceKind::Ble(device) => self.connect_ble(&descriptor.info, device).await,
        }
    }

    fn enumerate_usb_devices(&self) -> Result<Vec<MidiDeviceDescriptor>> {
        let midi_output = MidiOutput::new(CLIENT_NAME)
            .context("failed to initialize MIDI output for enumeration")?;
        let mut descriptors = Vec::new();
        for port in midi_output.ports() {
            let port_name = midi_output
                .port_name(&port)
                .unwrap_or_else(|_| "Unknown MIDI Output".to_string());
            let port_id = port.id();
            let device_id = Uuid::new_v5(&USB_NAMESPACE, port_id.as_bytes());
            let info = MidiSinkInfo::with_id(device_id, port_name.clone(), MidiTransport::Usb);
            descriptors.push(MidiDeviceDescriptor {
                info,
                kind: DeviceKind::Usb(UsbDevice { port_id, port_name }),
            });
        }
        Ok(descriptors)
    }

    async fn enumerate_ble_devices(
        &self,
        manager: &BtleManager,
    ) -> Result<Vec<MidiDeviceDescriptor>> {
        let mut descriptors = Vec::new();
        let adapters = manager
            .adapters()
            .await
            .context("failed to retrieve BLE adapters")?;

        if adapters.is_empty() {
            return Ok(descriptors);
        }

        for adapter in &adapters {
            if let Err(err) = adapter.start_scan(ScanFilter::default()).await {
                log::warn!("failed to start BLE scan: {err}");
            }
        }

        time::sleep(SCAN_TIMEOUT).await;

        for adapter in &adapters {
            if let Err(err) = adapter.stop_scan().await {
                log::debug!("failed to stop BLE scan: {err}");
            }

            let peripherals = match adapter.peripherals().await {
                Ok(peripherals) => peripherals,
                Err(err) => {
                    log::warn!("failed to list peripherals: {err}");
                    continue;
                }
            };

            for peripheral in peripherals {
                if !is_midi_candidate(&peripheral).await {
                    continue;
                }
                let name = peripheral_name(&peripheral).await;
                let peripheral_id = peripheral.id();
                let unique_key = format!("{}::{}", adapter_key(adapter).await, peripheral_id);
                let device_id = Uuid::new_v5(&BLE_NAMESPACE, unique_key.as_bytes());
                let info = MidiSinkInfo::with_id(device_id, name.clone(), MidiTransport::Bluetooth);
                descriptors.push(MidiDeviceDescriptor {
                    info,
                    kind: DeviceKind::Ble(BleDevice {
                        adapter: adapter.clone(),
                        peripheral_id,
                        name,
                    }),
                });
            }
        }

        Ok(descriptors)
    }

    async fn connect_usb(&self, _info: &MidiSinkInfo, device: UsbDevice) -> Result<SharedMidiSink> {
        let midi_output = MidiOutput::new(CLIENT_NAME)
            .context("failed to initialize MIDI output for connection")?;

        let port = midi_output
            .ports()
            .into_iter()
            .find(|port| port.id() == device.port_id)
            .with_context(|| {
                format!(
                    "MIDI output port {} is no longer available",
                    device.port_name
                )
            })?;

        let connection = midi_output
            .connect(&port, CLIENT_NAME)
            .map_err(|err| anyhow!("failed to connect to MIDI output port: {}", err))?;

        let sink = Arc::new(MidirSink {
            connection: Mutex::new(connection),
        });

        Ok(sink as SharedMidiSink)
    }

    async fn connect_ble(&self, _info: &MidiSinkInfo, device: BleDevice) -> Result<SharedMidiSink> {
        let peripheral = device
            .adapter
            .peripheral(&device.peripheral_id)
            .await
            .context("failed to retrieve BLE peripheral")?;

        if !peripheral.is_connected().await.unwrap_or(false) {
            peripheral
                .connect()
                .await
                .context("failed to connect to BLE MIDI device")?;
        }

        peripheral
            .discover_services()
            .await
            .context("failed to discover BLE services")?;

        let characteristic = peripheral
            .characteristics()
            .into_iter()
            .find(|c| c.uuid == BLE_MIDI_CHARACTERISTIC_UUID)
            .ok_or_else(|| anyhow!("BLE MIDI characteristic not found on {}", device.name))?;

        let sink = Arc::new(BleMidiSink {
            peripheral,
            characteristic,
            write_type: WriteType::WithoutResponse,
            write_lock: Mutex::new(()),
        });

        Ok(sink as SharedMidiSink)
    }
}

struct MidirSink {
    connection: Mutex<MidiOutputConnection>,
}

#[async_trait::async_trait]
impl MidiSink for MidirSink {
    async fn send(&self, data: &[u8]) -> Result<()> {
        let mut connection = self.connection.lock().await;
        connection
            .send(data)
            .map_err(|err| anyhow!("failed to send MIDI message: {err}"))
    }
}

struct BleMidiSink {
    peripheral: Peripheral,
    characteristic: Characteristic,
    write_type: WriteType,
    write_lock: Mutex<()>,
}

#[async_trait::async_trait]
impl MidiSink for BleMidiSink {
    async fn send(&self, data: &[u8]) -> Result<()> {
        let packet = pack_ble_midi_message(data);
        let _guard = self.write_lock.lock().await;
        self.peripheral
            .write(&self.characteristic, &packet, self.write_type)
            .await
            .map_err(|err| anyhow!("failed to send BLE MIDI data: {err}"))
    }
}

fn pack_ble_midi_message(data: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(data.len() + 1);
    packet.push(0x80); // Timestamp with zero offset.
    packet.extend_from_slice(data);
    packet
}

async fn is_midi_candidate(peripheral: &Peripheral) -> bool {
    match peripheral.properties().await {
        Ok(Some(properties)) => {
            if properties
                .services
                .iter()
                .any(|uuid| *uuid == BLE_MIDI_SERVICE_UUID)
            {
                return true;
            }
            if let Some(name) = properties.local_name {
                name.to_lowercase().contains("midi")
            } else {
                false
            }
        }
        Ok(None) => false,
        Err(err) => {
            log::debug!("unable to read properties for BLE peripheral: {err}");
            false
        }
    }
}

async fn adapter_key(adapter: &Adapter) -> String {
    adapter
        .adapter_info()
        .await
        .unwrap_or_else(|_| "adapter".into())
}

async fn peripheral_name(peripheral: &Peripheral) -> String {
    if let Ok(Some(properties)) = peripheral.properties().await {
        if let Some(name) = properties.local_name {
            return name;
        }
    }
    format!("BLE Device {}", peripheral.id())
}
