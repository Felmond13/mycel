// Entry point: register every component globally (templates reference each
// other by kebab-case tag), start polling + routing, mount.

import { createApp } from "./vue.js";
import { startPolling } from "./store.js";
import { startRouter } from "./router.js";

import AppShell from "./components/AppShell.js";
import Sidebar from "./components/Sidebar.js";
import NetworkCanvas from "./components/NetworkCanvas.js";
import ToastHost from "./components/ToastHost.js";
import ConfirmDialog from "./components/ConfirmDialog.js";
import LogsPanel from "./components/LogsPanel.js";

import AnimatedNumber from "./components/AnimatedNumber.js";
import SkeletonBlock from "./components/SkeletonBlock.js";
import StatusDot from "./components/StatusDot.js";
import EmptyState from "./components/EmptyState.js";
import StatCard from "./components/StatCard.js";
import RefAvatar from "./components/RefAvatar.js";
import RefName from "./components/RefName.js";
import Sparkline from "./components/Sparkline.js";
import ContainerExpansion from "./components/ContainerExpansion.js";
import StartDialog from "./components/StartDialog.js";
import ShareDialog from "./components/ShareDialog.js";
import DeployDialog from "./components/DeployDialog.js";
import ImportDialog from "./components/ImportDialog.js";

import AppsPage from "./components/AppsPage.js";
import ContainersPage from "./components/ContainersPage.js";
import StacksPage from "./components/StacksPage.js";
import StackView from "./components/StackView.js";
import StackEditor from "./components/StackEditor.js";
import LibraryPage from "./components/LibraryPage.js";
import LibraryDetail from "./components/LibraryDetail.js";
import StorePage from "./components/StorePage.js";
import IngestPage from "./components/IngestPage.js";
import DiffPage from "./components/DiffPage.js";
import SearchPage from "./components/SearchPage.js";
import DoctorPage from "./components/DoctorPage.js";

const app = createApp(AppShell);

app.component("side-bar", Sidebar);
app.component("network-canvas", NetworkCanvas);
app.component("toast-host", ToastHost);
app.component("confirm-dialog", ConfirmDialog);
app.component("logs-panel", LogsPanel);

app.component("anim-num", AnimatedNumber);
app.component("skeleton-block", SkeletonBlock);
app.component("status-dot", StatusDot);
app.component("empty-state", EmptyState);
app.component("stat-card", StatCard);
app.component("ref-avatar", RefAvatar);
app.component("ref-name", RefName);
app.component("spark-line", Sparkline);
app.component("container-expansion", ContainerExpansion);
app.component("start-dialog", StartDialog);
app.component("share-dialog", ShareDialog);
app.component("deploy-dialog", DeployDialog);
app.component("import-dialog", ImportDialog);

app.component("apps-page", AppsPage);
app.component("containers-page", ContainersPage);
app.component("stacks-page", StacksPage);
app.component("stack-view", StackView);
app.component("stack-editor", StackEditor);
app.component("library-page", LibraryPage);
app.component("library-detail", LibraryDetail);
app.component("store-page", StorePage);
app.component("ingest-page", IngestPage);
app.component("diff-page", DiffPage);
app.component("search-page", SearchPage);
app.component("doctor-page", DoctorPage);

startPolling();
startRouter();
app.mount("#app");
