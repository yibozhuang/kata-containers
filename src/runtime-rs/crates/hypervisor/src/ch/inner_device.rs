// Copyright (c) 2019-2022 Alibaba Cloud
// Copyright (c) 2019-2022 Ant Group
// Copyright (c) 2022 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0

use super::inner::CloudHypervisorInner;
use crate::device::{Device, ShareFsDeviceConfig};
use crate::HybridVsockConfig;
use crate::VmmState;
use anyhow::{anyhow, Context, Result};
use ch_config::ch_api::cloud_hypervisor_vm_fs_add;
use ch_config::{FsConfig, PmemConfig};
use safe_path::scoped_join;
use std::convert::TryFrom;
use std::path::PathBuf;

const VIRTIO_FS: &str = "virtio-fs";

impl CloudHypervisorInner {
    pub(crate) async fn add_device(&mut self, device: Device) -> Result<()> {
        if self.state != VmmState::VmRunning {
            let mut devices: Vec<Device> = if let Some(devices) = self.pending_devices.take() {
                devices
            } else {
                vec![]
            };

            devices.insert(0, device);

            self.pending_devices = Some(devices);

            return Ok(());
        }

        self.handle_add_device(device).await?;

        Ok(())
    }

    async fn handle_add_device(&mut self, device: Device) -> Result<()> {
        match device {
            Device::ShareFsDevice(cfg) => self.handle_share_fs_device(cfg).await,
            Device::HybridVsock(cfg) => self.handle_hvsock_device(&cfg).await,
            _ => return Err(anyhow!("unhandled device: {:?}", device)),
        }
    }

    /// Add the device that were requested to be added before the VMM was
    /// started.
    #[allow(dead_code)]
    pub(crate) async fn handle_pending_devices_after_boot(&mut self) -> Result<()> {
        if self.state != VmmState::VmRunning {
            return Err(anyhow!(
                "cannot handle pending devices with VMM state {:?}",
                self.state
            ));
        }

        if let Some(mut devices) = self.pending_devices.take() {
            while let Some(dev) = devices.pop() {
                self.add_device(dev).await.context("add_device")?;
            }
        }

        Ok(())
    }

    pub(crate) async fn remove_device(&mut self, _device: Device) -> Result<()> {
        Ok(())
    }

    async fn handle_share_fs_device(&mut self, cfg: ShareFsDeviceConfig) -> Result<()> {
        if cfg.fs_type != VIRTIO_FS {
            return Err(anyhow!("cannot handle share fs type: {:?}", cfg.fs_type));
        }

        let socket = self
            .api_socket
            .as_ref()
            .ok_or("missing socket")
            .map_err(|e| anyhow!(e))?;

        let num_queues: usize = if cfg.queue_num > 0 {
            cfg.queue_num as usize
        } else {
            1
        };

        let queue_size: u16 = if cfg.queue_num > 0 {
            u16::try_from(cfg.queue_size)?
        } else {
            1024
        };

        let socket_path = if cfg.sock_path.starts_with('/') {
            PathBuf::from(cfg.sock_path)
        } else {
            scoped_join(&self.vm_path, cfg.sock_path)?
        };

        let fs_config = FsConfig {
            tag: cfg.mount_tag,
            socket: socket_path,
            num_queues,
            queue_size,
            ..Default::default()
        };

        let response = cloud_hypervisor_vm_fs_add(
            socket.try_clone().context("failed to clone socket")?,
            fs_config,
        )
        .await?;

        if let Some(detail) = response {
            debug!(sl!(), "fs add response: {:?}", detail);
        }

        Ok(())
    }

    async fn handle_hvsock_device(&mut self, _cfg: &HybridVsockConfig) -> Result<()> {
        Ok(())
    }

    pub(crate) async fn get_shared_fs_devices(&mut self) -> Result<Option<Vec<FsConfig>>> {
        let pending_root_devices = self.pending_devices.take();

        let mut root_devices = Vec::<FsConfig>::new();

        if let Some(devices) = pending_root_devices {
            for dev in devices {
                match dev {
                    Device::ShareFsDevice(dev) => {
                        let settings = ShareFsSettings::new(dev, self.vm_path.clone());

                        let fs_cfg = FsConfig::try_from(settings)?;

                        root_devices.push(fs_cfg);
                    }
                    _ => continue,
                };
            }

            Ok(Some(root_devices))
        } else {
            Ok(None)
        }
    }

    pub(crate) async fn get_boot_file(&mut self) -> Result<PathBuf> {
        if let Some(ref config) = self.config {
            let boot_info = &config.boot_info;

            let file = if !boot_info.initrd.is_empty() {
                boot_info.initrd.clone()
            } else if !boot_info.image.is_empty() {
                boot_info.image.clone()
            } else {
                return Err(anyhow!("missing boot file (no image or initrd)"));
            };

            Ok(PathBuf::from(file))
        } else {
            Err(anyhow!("no hypervisor config"))
        }
    }

    pub(crate) async fn get_pmem_devices(&mut self) -> Result<Option<Vec<PmemConfig>>> {
        let file = self.get_boot_file().await?;

        let pmem_cfg = PmemConfig {
            file,
            size: None,
            iommu: false,
            discard_writes: true,
            id: None,
            pci_segment: 0,
        };

        let pmem_devices = vec![pmem_cfg];

        Ok(Some(pmem_devices))
    }
}

#[derive(Debug)]
pub struct ShareFsSettings {
    cfg: ShareFsDeviceConfig,
    vm_path: String,
}

impl ShareFsSettings {
    pub fn new(cfg: ShareFsDeviceConfig, vm_path: String) -> Self {
        ShareFsSettings { cfg, vm_path }
    }
}

impl TryFrom<ShareFsSettings> for FsConfig {
    type Error = anyhow::Error;

    fn try_from(settings: ShareFsSettings) -> Result<Self, Self::Error> {
        let cfg = settings.cfg;
        let vm_path = settings.vm_path;

        let num_queues: usize = if cfg.queue_num > 0 {
            cfg.queue_num as usize
        } else {
            1
        };

        let queue_size: u16 = if cfg.queue_num > 0 {
            u16::try_from(cfg.queue_size)?
        } else {
            1024
        };

        let socket_path = if cfg.sock_path.starts_with('/') {
            PathBuf::from(cfg.sock_path)
        } else {
            PathBuf::from(vm_path).join(cfg.sock_path)
        };

        let fs_cfg = FsConfig {
            tag: cfg.mount_tag,
            socket: socket_path,
            num_queues,
            queue_size,
            ..Default::default()
        };

        Ok(fs_cfg)
    }
}
