package me.weishu.kernelsu.ui.screen.settings

import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.unit.Dp
import androidx.lifecycle.compose.LifecycleResumeEffect
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import me.weishu.kernelsu.ui.navigation3.Navigator
import me.weishu.kernelsu.ui.navigation3.Route
import me.weishu.kernelsu.ui.viewmodel.SettingsViewModel

@Composable
fun SettingPager(
    navigator: Navigator,
    bottomInnerPadding: Dp
) {
    val viewModel = viewModel<SettingsViewModel>()
    val uiState by viewModel.uiState.collectAsStateWithLifecycle()

    LifecycleResumeEffect(Unit) {
        viewModel.refresh()
        onPauseOrDispose { }
    }

    val actions = SettingsScreenActions(
        onSetCheckUpdate = viewModel::setCheckUpdate,
        onSetCheckModuleUpdate = viewModel::setCheckModuleUpdate,
        onOpenProfileTemplate = { navigator.push(Route.AppProfileTemplate) },
        onSetSuCompatMode = viewModel::setSuCompatMode,
        onSetKernelUmountEnabled = viewModel::setKernelUmountEnabled,
        onSetSelinuxHideEnabled = viewModel::setSelinuxHideEnabled,
        onSetSulogEnabled = viewModel::setSulogEnabled,
        onSetAdbRootEnabled = viewModel::setAdbRootEnabled,
        onSetDefaultUmountModules = viewModel::setDefaultUmountModules,
        onSetEnableWebDebugging = viewModel::setEnableWebDebugging,
        onSetAutoJailbreak = viewModel::setAutoJailbreak,
        onOpenAbout = { navigator.push(Route.About) },
    )

    SettingPagerMaterial(uiState, actions, bottomInnerPadding)
}
