// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (c) 2026 Dafeiyu Remote contributors

use anyhow::{bail, Context, Result};
use dfy_vdisplay::{
    capability, create_backend, VirtualDisplayBackend, VirtualDisplayInfo, VirtualDisplaySpec,
};

const MAX_AGENT_DISPLAYS: u32 = 16;

pub struct ManagedVirtualDisplays {
    backend: Box<dyn VirtualDisplayBackend>,
    ids: Vec<u32>,
    selected: u32,
}

impl ManagedVirtualDisplays {
    pub fn create(count: u32, selected: u32, spec: &VirtualDisplaySpec) -> Result<Self> {
        if !(1..=MAX_AGENT_DISPLAYS).contains(&count) {
            bail!("虚拟显示数量必须在 1..={MAX_AGENT_DISPLAYS}（收到 {count}）");
        }
        if selected >= count {
            bail!("虚拟显示器索引 {selected} 超出范围（共 {count} 个）");
        }
        spec.validate()?;
        let backend = create_backend().context("创建虚拟显示后端失败")?;
        if !backend.is_available() {
            bail!(
                "当前平台虚拟显示后端不可用：{}",
                dfy_vdisplay::platform_name()
            );
        }
        if count > 1 && backend.capabilities() & capability::MULTI_MONITOR == 0 {
            bail!("当前虚拟显示后端不支持多个独立显示器");
        }
        let mut ids = Vec::with_capacity(count as usize);
        for index in 0..count {
            let mut current = spec.clone();
            if current.name_hint.is_empty() {
                current.name_hint = format!("dfy-virtual-{}", index + 1);
            } else {
                current.name_hint = format!("{}-{}", current.name_hint, index + 1);
            }
            current.validate()?;
            ids.push(backend.create(&current).with_context(|| {
                format!(
                    "创建第 {} 个虚拟显示失败（{}x{}）",
                    index + 1,
                    current.width,
                    current.height
                )
            })?);
        }
        let selected = ids[selected as usize];
        Ok(Self {
            backend,
            ids,
            selected,
        })
    }

    pub fn ids(&self) -> &[u32] {
        &self.ids
    }

    pub fn selected_id(&self) -> u32 {
        self.selected
    }

    pub fn refresh(&self) -> Result<VirtualDisplayInfo> {
        let displays = self.backend.list()?;
        let selected = displays
            .into_iter()
            .find(|display| display.id == self.selected)
            .with_context(|| format!("虚拟显示 {} 已从注册表消失", self.selected))?;
        if !selected.attached {
            bail!("虚拟显示 {} 当前未连接", self.selected);
        }
        Ok(selected)
    }

    pub fn capture_endpoint(&self) -> Result<String> {
        self.refresh()?;
        self.backend
            .capture_endpoint(self.selected)?
            .with_context(|| "该虚拟显示后端尚未提供采集端点映射")
    }
}

impl Drop for ManagedVirtualDisplays {
    fn drop(&mut self) {
        for id in self.ids.iter().rev() {
            if let Err(err) = self.backend.destroy(*id) {
                tracing::warn!(display = *id, error = %err, "清理虚拟显示失败");
            }
        }
    }
}

pub fn list() -> Result<Vec<VirtualDisplayInfo>> {
    create_backend()?.list()
}

pub fn update(id: u32, spec: &VirtualDisplaySpec) -> Result<()> {
    create_backend()?.update(id, spec)
}

pub fn destroy(id: u32) -> Result<()> {
    create_backend()?.destroy(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_counts_before_touching_the_backend() {
        let spec = VirtualDisplaySpec::hd_1080p();
        assert!(ManagedVirtualDisplays::create(0, 0, &spec).is_err());
        assert!(ManagedVirtualDisplays::create(17, 0, &spec).is_err());
        assert!(ManagedVirtualDisplays::create(2, 2, &spec).is_err());
    }
}
