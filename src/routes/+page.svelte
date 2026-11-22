<script lang="ts">
	import type { DeviceInfo } from "$lib/DeviceInfo";
	import type { Profile } from "$lib/Profile";

	import { t } from "$lib/i18n";
	import { initPortBase } from "$lib/ports";
	import { inspectedParentAction } from "$lib/propertyInspector";
	import { PRODUCT_NAME, actionList, deviceSelector, profileManager } from "$lib/singletons";

	import { listen } from "@tauri-apps/api/event";
	import type { UnlistenFn } from "@tauri-apps/api/event";
	import { onMount } from "svelte";

	import ActionList from "../components/ActionList.svelte";
	import DeviceSelector from "../components/DeviceSelector.svelte";
	import DeviceView from "../components/DeviceView.svelte";
	import NoDevicesDetected from "../components/NoDevicesDetected.svelte";
	import ParentActionView from "../components/ParentActionView.svelte";
	import PluginManager from "../components/PluginManager.svelte";
	import ProfileManager from "../components/ProfileManager.svelte";
	import PropertyInspectorView from "../components/PropertyInspectorView.svelte";
	import SettingsView from "../components/SettingsView.svelte";

	type RecoveryState = "resetting" | "restarting" | "recovered" | "failed";

	let devices: { [id: string]: DeviceInfo } = {};
	let selectedDevice: string;
	let selectedProfiles: { [id: string]: Profile } = {};
	let recovery: { state: RecoveryState; name: string } | null = null;
	let recoveryTimer: ReturnType<typeof setTimeout> | undefined;

	initPortBase();

	onMount(() => {
		let destroyed = false;
		let unlisten: UnlistenFn | undefined;

		listen<{ state: RecoveryState; name: string }>("device_recovery", ({ payload }) => {
			clearTimeout(recoveryTimer);
			recovery = payload;
			if (payload.state === "recovered") {
				recoveryTimer = setTimeout(() => (recovery = null), 4000);
			}
		}).then((fn) => {
			if (destroyed) fn();
			else unlisten = fn;
		});

		return () => {
			destroyed = true;
			unlisten?.();
			clearTimeout(recoveryTimer);
		};
	});
</script>

<svelte:window on:dragover={(event) => event.preventDefault()} on:drop={(event) => event.preventDefault()} />

<div class="flex flex-row h-screen">
	<div class="flex flex-col grow min-w-0">
		<nav class="flex flex-row justify-between items-center p-3" class:hidden={$inspectedParentAction}>
			<div class="flex flex-col items-start space-y-1">
				<DeviceSelector bind:devices bind:value={selectedDevice} bind:selectedProfiles bind:this={$deviceSelector} />
				{#key selectedDevice}
					{#if selectedDevice && devices[selectedDevice]}
						<ProfileManager bind:device={devices[selectedDevice]} bind:profile={selectedProfiles[selectedDevice]} bind:this={$profileManager} />
					{/if}
				{/key}
			</div>

			<div class="flex flex-row items-center space-x-2" class:mr-4={Object.keys(devices).length > 0}>
				<PluginManager />
				<SettingsView />
			</div>
		</nav>

		{#if Object.keys(devices).length > 0 && selectedProfiles}
			{#if $inspectedParentAction}
				<ParentActionView bind:profile={selectedProfiles[selectedDevice]} />
			{/if}

			{#each Object.entries(devices) as [id, device]}
				{#if device && selectedProfiles[id]}
					<DeviceView bind:device bind:profile={selectedProfiles[id]} bind:selectedDevice />
				{/if}
			{/each}

			{#if selectedProfiles[selectedDevice]}
				<PropertyInspectorView bind:device={devices[selectedDevice]} bind:profile={selectedProfiles[selectedDevice]} />
			{/if}
		{:else}
			<NoDevicesDetected />
		{/if}
	</div>

	<ActionList bind:this={$actionList} />
</div>

{#if recovery}
	<div
		role="status"
		class="fixed bottom-4 right-4 z-50 max-w-sm rounded-lg border border-neutral-600 bg-neutral-800 p-3 text-sm text-neutral-200 shadow-lg"
	>
		<p>{$t(`device_recovery.${recovery.state}`, { name: recovery.name, PRODUCT_NAME })}</p>
		{#if recovery.state === "failed"}
			<button
				class="mt-2 px-2 py-1 text-xs bg-neutral-700 hover:bg-neutral-600 transition-colors border border-neutral-600 rounded-lg"
				on:click={() => (recovery = null)}
			>
				{$t("device_recovery.dismiss")}
			</button>
		{/if}
	</div>
{/if}
