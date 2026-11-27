export declare function input(): string;
export declare function record(value: string): void;
export declare function mask(value: string): string;
export declare function approved(value: string): boolean;
export declare function approvedAsync(value: string): Promise<boolean>;
export declare function opaque(value: string): string;
export declare function copy(value: string): string;
export declare function maskInPlace(target: { value: string }): void;
export declare function copyTo(value: string, target: { value: string }): void;
