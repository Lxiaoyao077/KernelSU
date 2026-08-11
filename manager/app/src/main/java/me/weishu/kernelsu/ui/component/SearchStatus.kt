package me.weishu.kernelsu.ui.component

import androidx.compose.runtime.Stable

@Stable
data class SearchStatus(
    val searchText: String = "",
    val resultStatus: ResultStatus = ResultStatus.DEFAULT,
) {
    enum class ResultStatus { DEFAULT, LOAD, EMPTY, SHOW }
}
