package me.weishu.kernelsu.ui.util

import android.net.Uri
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.onEach
import me.weishu.kernelsu.BuildConfig
import me.weishu.kernelsu.ksuApp
import me.weishu.kernelsu.ui.util.module.LatestVersionInfo
import okhttp3.CacheControl
import okhttp3.Request

/**
 * @author weishu
 * @date 2023/6/22.
 */

// Update source: this fork, not upstream tiann/KernelSU.
private const val REPOSITORY = "Lxiaoyao077/KernelSU"
private const val WORKFLOW_FILE = "build-manager.yml"
private const val BRANCH = "main"
private const val RELEASE_ARTIFACT = "manager-gradle"

// sync with manager/build.gradle.kts: versionCode = 30000 + git commit count
private const val VERSION_CODE_OFFSET = 30000

// APK names follow "v3.3.0-<commitCount>-g<sha>" (KowSU style), e.g.
// KernelSU-Manager_v3.3.0-2600-gb6b6fe77_release.apk
private val apkVersionPattern = Regex("v[\\w.]+-(\\d+)-g[0-9a-f]+")
// GitHub API pagination: extract the total commit count from the Link header
private val commitCountLinkPattern = Regex("""[?&]page=(\d+)>; rel="last"""")

suspend fun download(
    url: String,
    fileName: String,
    onDownloaded: (Uri) -> Unit = {},
    onDownloading: () -> Unit = {},
    onProgress: (Int) -> Unit = {}
) {
    onDownloading()

    val downloadId = DownloadManager.enqueue(
        context = ksuApp,
        url = url,
        fileName = fileName,
        onCompleted = onDownloaded,
    )

    DownloadManager.downloads
        .onEach { map -> map[downloadId]?.let { onProgress(it.progress) } }
        .first { map ->
            val status = map[downloadId]?.status
            status == DownloadManager.Status.COMPLETED ||
                status == DownloadManager.Status.FAILED
        }
}

/**
 * Check for manager updates from this fork.
 *
 * 1. STABLE: the latest GitHub Release of Lxiaoyao077/KernelSU.
 * 2. BETA/CI: the most recent successful build-manager run on `main`
 *    (ReSukiSU-style nightly detection), so updates are found even when no
 *    Release has been published yet.
 */
fun checkNewVersion(): LatestVersionInfo {
    if (!isNetworkAvailable(ksuApp)) return LatestVersionInfo()
    checkReleaseUpdate()?.let { return it }
    checkCiUpdate()?.let { return it }
    return LatestVersionInfo()
}

private fun checkReleaseUpdate(): LatestVersionInfo? {
    val release = requestJson("https://api.github.com/repos/$REPOSITORY/releases/latest")
        ?: return null
    val changelog = release.optString("body")
    val assets = release.optJSONArray("assets") ?: return null

    for (i in 0 until assets.length()) {
        val asset = assets.optJSONObject(i) ?: continue
        val match = apkVersionPattern.find(asset.optString("name")) ?: continue
        val commitCount = match.groupValues[1].toIntOrNull() ?: continue
        val downloadUrl = asset.optString("browser_download_url")
        if (downloadUrl.isBlank()) continue
        return LatestVersionInfo(
            versionCode = VERSION_CODE_OFFSET + commitCount,
            downloadUrl = downloadUrl,
            changelog = changelog,
        )
    }
    return null
}

private fun checkCiUpdate(): LatestVersionInfo? {
    val runs = requestJson(
        "https://api.github.com/repos/$REPOSITORY/actions/workflows/$WORKFLOW_FILE/runs" +
            "?branch=$BRANCH&status=success&per_page=1&event=push"
    )?.optJSONArray("workflow_runs") ?: return null
    val run = runs.optJSONObject(0) ?: return null
    val runId = run.optLong("id", -1L)
    val headSha = run.optString("head_sha")
    if (runId <= 0L || headSha.isBlank()) return null

    val commitCount = requestCommitCount(headSha) ?: return null
    val versionCode = VERSION_CODE_OFFSET + commitCount
    // Only surface newer builds than the currently installed manager.
    if (versionCode <= BuildConfig.VERSION_CODE) return null

    return LatestVersionInfo(
        versionCode = versionCode,
        downloadUrl = "https://nightly.link/$REPOSITORY/actions/runs/$runId/$RELEASE_ARTIFACT.zip",
        changelog = run.optJSONObject("head_commit")?.optString("message").orEmpty(),
    )
}

private fun requestJson(url: String): org.json.JSONObject? {
    return ksuApp.okhttpClient.newCall(githubRequest(url).build()).execute().use { response ->
        if (!response.isSuccessful) return null
        val body = response.body?.string() ?: return null
        org.json.JSONObject(body)
    }
}

private fun requestCommitCount(commitSha: String): Int? {
    val url = "https://api.github.com/repos/$REPOSITORY/commits?sha=$commitSha&per_page=1"
    return ksuApp.okhttpClient.newCall(githubRequest(url).build()).execute().use { response ->
        if (!response.isSuccessful) return null
        commitCountLinkPattern.find(response.header("Link").orEmpty())
            ?.groupValues
            ?.getOrNull(1)
            ?.toIntOrNull()
            ?: 1
    }
}

private fun githubRequest(url: String): Request.Builder =
    Request.Builder()
        .url(url)
        .header("Accept", "application/vnd.github+json")
        .cacheControl(CacheControl.FORCE_NETWORK)
